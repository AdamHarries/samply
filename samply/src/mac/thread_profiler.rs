use framehop::FrameAddress;
use fxprof_processed_profile::{CpuDelta, Profile, ThreadHandle, Timestamp};
use mach::mach_types::thread_act_t;
use mach::port::mach_port_t;

use std::mem;

use crate::shared::types::{StackFrame, StackMode};
use crate::shared::unresolved_samples::{UnresolvedSamples, UnresolvedStacks};

use super::error::SamplingError;
use super::kernel_error::{self, IntoResult, KernelError};
use super::proc_maps::{get_backtrace, ForeignMemory, StackwalkerRef};
use super::thread_act::thread_info;
use super::thread_info::time_value;
use super::thread_info::{
    thread_basic_info_data_t, thread_extended_info_data_t, thread_identifier_info_data_t,
    thread_info_t, THREAD_BASIC_INFO, THREAD_BASIC_INFO_COUNT, THREAD_EXTENDED_INFO,
    THREAD_EXTENDED_INFO_COUNT, THREAD_IDENTIFIER_INFO, THREAD_IDENTIFIER_INFO_COUNT,
};

pub struct ThreadProfiler {
    thread_act: thread_act_t,
    name: Option<String>,
    tid: u32,
    profile_thread: ThreadHandle,
    tick_count: usize,
    stack_memory: ForeignMemory,
    previous_sample_cpu_time_us: u64,
    ignored_errors: Vec<SamplingError>,
}

impl ThreadProfiler {
    pub fn new(
        task: mach_port_t,
        tid: u32,
        profile_thread: ThreadHandle,
        thread_act: thread_act_t,
    ) -> Self {
        ThreadProfiler {
            thread_act,
            tid,
            name: None,
            profile_thread,
            tick_count: 0,
            stack_memory: ForeignMemory::new(task),
            previous_sample_cpu_time_us: 0,
            ignored_errors: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sample(
        &mut self,
        stackwalker: StackwalkerRef,
        now: Timestamp,
        now_mono: u64,
        profile: &mut Profile,
        stack_scratch_buffer: &mut Vec<FrameAddress>,
        unresolved_stacks: &mut UnresolvedStacks,
        unresolved_samples: &mut UnresolvedSamples,
    ) -> Result<bool, SamplingError> {
        let result = self.sample_impl(
            stackwalker,
            now,
            now_mono,
            profile,
            stack_scratch_buffer,
            unresolved_stacks,
            unresolved_samples,
        );
        match result {
            Ok(()) => Ok(true),
            Err(SamplingError::ThreadTerminated(_, _)) => Ok(false),
            Err(err @ SamplingError::Ignorable(_, _)) => {
                self.ignored_errors.push(err);
                if self.ignored_errors.len() >= 10 {
                    println!(
                        "Treating thread \"{}\" [tid: {}] as terminated after 10 unknown errors:",
                        self.name.as_deref().unwrap_or("<unknown"),
                        self.tid
                    );
                    println!("{:#?}", self.ignored_errors);
                    Ok(false)
                } else {
                    // Pretend that sampling worked and that the thread is still alive.
                    Ok(true)
                }
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_impl(
        &mut self,
        stackwalker: StackwalkerRef,
        now: Timestamp,
        now_mono: u64,
        profile: &mut Profile,
        stack_scratch_buffer: &mut Vec<FrameAddress>,
        unresolved_stacks: &mut UnresolvedStacks,
        unresolved_samples: &mut UnresolvedSamples,
    ) -> Result<(), SamplingError> {
        self.tick_count += 1;

        if self.name.is_none() && self.tick_count % 10 == 1 {
            self.name = get_thread_name(self.thread_act)?;
            if let Some(name) = &self.name {
                profile.set_thread_name(self.profile_thread, name);
            }
        }

        let cpu_time_us = get_thread_cpu_time_since_thread_start(self.thread_act)?;
        let cpu_time_us = cpu_time_us.0 + cpu_time_us.1;
        let cpu_delta_us = cpu_time_us - self.previous_sample_cpu_time_us;
        let cpu_delta = CpuDelta::from_micros(cpu_delta_us);

        if !cpu_delta.is_zero() || self.tick_count == 0 {
            stack_scratch_buffer.clear();
            get_backtrace(
                stackwalker,
                &mut self.stack_memory,
                self.thread_act,
                stack_scratch_buffer,
            )?;

            let frames = stack_scratch_buffer.iter().rev().map(|f| match f {
                FrameAddress::InstructionPointer(address) => {
                    StackFrame::InstructionPointer(*address, StackMode::User)
                }
                FrameAddress::ReturnAddress(address) => {
                    StackFrame::ReturnAddress((*address).into(), StackMode::User)
                }
            });
            let stack = unresolved_stacks.convert(frames);
            unresolved_samples.add_sample(self.profile_thread, now, now_mono, stack, cpu_delta, 1);
        } else {
            // No CPU time elapsed since just before the last time we grabbed a stack.
            // Assume that the thread has done literally zero work and could not have changed
            // its stack. This considerably reduces the overhead from sampling idle threads.
            //
            // More specifically, we hit this path after the following order of events
            //  - sample n-1:
            //     - query cpu time, call it A
            //     - pause the thread
            //     - walk the stack
            //     - resume the thread
            //  - sleep till next sample
            //  - sample n:
            //     - query cpu time, notice it is still the same as A
            //     - add_sample_same_stack with stack from previous sample
            //
            unresolved_samples.add_sample_same_stack_zero_cpu(
                self.profile_thread,
                now,
                now_mono,
                1,
            );
        }

        self.previous_sample_cpu_time_us = cpu_time_us;

        Ok(())
    }

    pub fn notify_dead(&mut self, end_time: Timestamp, profile: &mut Profile) {
        profile.set_thread_end_time(self.profile_thread, end_time);
        self.stack_memory.clear();
    }
}

/// Returns (tid, is_libdispatch_thread)
pub fn get_thread_id(thread_act: thread_act_t) -> kernel_error::Result<(u32, bool)> {
    let mut identifier_info_data: thread_identifier_info_data_t = unsafe { mem::zeroed() };
    let mut count = THREAD_IDENTIFIER_INFO_COUNT;
    unsafe {
        thread_info(
            thread_act,
            THREAD_IDENTIFIER_INFO,
            &mut identifier_info_data as *mut _ as thread_info_t,
            &mut count,
        )
    }
    .into_result()?;

    // This used to check dispatch_qaddr != 0, but it looks like this can happen
    // even for non-libdispatch threads, for example it happens for rust threads
    // such as the perfrecord sampler thread.
    let is_libdispatch_thread = false; // TODO

    Ok((identifier_info_data.thread_id as u32, is_libdispatch_thread))
}

fn get_thread_name(thread_act: thread_act_t) -> Result<Option<String>, SamplingError> {
    // Get the thread name.
    let mut extended_info_data: thread_extended_info_data_t = unsafe { mem::zeroed() };
    let mut count = THREAD_EXTENDED_INFO_COUNT;
    unsafe {
        thread_info(
            thread_act,
            THREAD_EXTENDED_INFO,
            &mut extended_info_data as *mut _ as thread_info_t,
            &mut count,
        )
    }
    .into_result()
    .map_err(|err| match err {
        KernelError::InvalidArgument
        | KernelError::MachSendInvalidDest
        | KernelError::Terminated => {
            SamplingError::ThreadTerminated("thread_info in get_thread_name", err)
        }
        err => {
            println!("thread_info in get_thread_name encountered unexpected error: {err:?}");
            SamplingError::Ignorable("thread_info in get_thread_name", err)
        }
    })?;

    let name = unsafe { std::ffi::CStr::from_ptr(extended_info_data.pth_name.as_ptr()) }
        .to_string_lossy()
        .to_string();
    Ok(if name.is_empty() { None } else { Some(name) })
}

// (user time, system time) in microseconds
fn get_thread_cpu_time_since_thread_start(
    thread_act: thread_act_t,
) -> Result<(u64, u64), SamplingError> {
    let mut basic_info_data: thread_basic_info_data_t = unsafe { mem::zeroed() };
    let mut count = THREAD_BASIC_INFO_COUNT;
    unsafe {
        thread_info(
            thread_act,
            THREAD_BASIC_INFO,
            &mut basic_info_data as *mut _ as thread_info_t,
            &mut count,
        )
    }
    .into_result()
    .map_err(|err| match err {
        KernelError::InvalidArgument
        | KernelError::MachSendInvalidDest
        | KernelError::Terminated => SamplingError::ThreadTerminated(
            "thread_info in get_thread_cpu_time_since_thread_start",
            err,
        ),
        err => {
            SamplingError::Ignorable("thread_info in get_thread_cpu_time_since_thread_start", err)
        }
    })?;

    Ok((
        time_value_to_microseconds(&basic_info_data.user_time),
        time_value_to_microseconds(&basic_info_data.system_time),
    ))
}

fn time_value_to_microseconds(tv: &time_value) -> u64 {
    tv.seconds as u64 * 1_000_000 + tv.microseconds as u64
}
