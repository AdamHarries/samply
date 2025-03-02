mod context_switch;
mod kernel_symbols;
mod object_rewriter;

use byteorder::{ByteOrder, LittleEndian};
use context_switch::{ContextSwitchHandler, OffCpuSampleGroup, ThreadContextSwitchData};
use debugid::{CodeId, DebugId};
use framehop::aarch64::UnwindRegsAarch64;
use framehop::x86_64::UnwindRegsX86_64;
use framehop::{FrameAddress, Module, ModuleSvmaInfo, ModuleUnwindData, TextByteData, Unwinder};
use fxprof_processed_profile::{
    CategoryColor, CounterHandle, CpuDelta, LibraryHandle, LibraryInfo, MarkerTiming,
    ProcessHandle, Profile, ReferenceTimestamp, SamplingInterval, ThreadHandle, Timestamp,
};
use linux_perf_data::linux_perf_event_reader;
use linux_perf_data::{AttributeDescription, DsoInfo, DsoKey, Endianness};
use linux_perf_event_reader::constants::{
    PERF_CONTEXT_MAX, PERF_REG_ARM64_LR, PERF_REG_ARM64_PC, PERF_REG_ARM64_SP, PERF_REG_ARM64_X29,
    PERF_REG_X86_BP, PERF_REG_X86_IP, PERF_REG_X86_SP,
};
use linux_perf_event_reader::{
    AttrFlags, CommOrExecRecord, CommonData, ContextSwitchRecord, ForkOrExitRecord, Mmap2FileId,
    Mmap2Record, MmapRecord, PerfEventType, RawData, RawDataU64, Regs, SampleRecord,
    SamplingPolicy, SoftwareCounterType,
};
use memmap2::Mmap;
use object::pe::{ImageNtHeaders32, ImageNtHeaders64};
use object::read::pe::{ImageNtHeaders, ImageOptionalHeader, PeFile};
use object::{
    FileKind, Object, ObjectSection, ObjectSegment, ObjectSymbol, SectionKind, SymbolKind,
};
use samply_symbols::{debug_id_for_object, DebugIdExt};
use wholesym::samply_symbols;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Debug;
use std::path::PathBuf;
use std::time::SystemTime;
use std::{ops::Range, path::Path};

use self::kernel_symbols::KernelSymbols;
use crate::shared::jit_category_manager::JitCategoryManager;
use crate::shared::jit_function_add_marker::JitFunctionAddMarker;
use crate::shared::jit_function_recycler::JitFunctionRecycler;
use crate::shared::jitdump_manager::JitDumpManager;
use crate::shared::lib_mappings::{LibMappingAdd, LibMappingInfo, LibMappingOp, LibMappingOpQueue};
use crate::shared::perf_map::try_load_perf_map;
use crate::shared::process_sample_data::{ProcessSampleData, RssStatMember};
use crate::shared::timestamp_converter::TimestampConverter;
use crate::shared::types::{StackFrame, StackMode};
use crate::shared::unresolved_samples::{
    UnresolvedSamples, UnresolvedStackHandle, UnresolvedStacks,
};
use crate::shared::utils::open_file_with_fallback;

pub trait ConvertRegs {
    type UnwindRegs;
    fn convert_regs(regs: &Regs) -> (u64, u64, Self::UnwindRegs);
    fn regs_mask() -> u64;
}

pub struct ConvertRegsX86_64;
impl ConvertRegs for ConvertRegsX86_64 {
    type UnwindRegs = UnwindRegsX86_64;
    fn convert_regs(regs: &Regs) -> (u64, u64, UnwindRegsX86_64) {
        let ip = regs.get(PERF_REG_X86_IP).unwrap();
        let sp = regs.get(PERF_REG_X86_SP).unwrap();
        let bp = regs.get(PERF_REG_X86_BP).unwrap();
        let regs = UnwindRegsX86_64::new(ip, sp, bp);
        (ip, sp, regs)
    }

    fn regs_mask() -> u64 {
        1 << PERF_REG_X86_IP | 1 << PERF_REG_X86_SP | 1 << PERF_REG_X86_BP
    }
}

pub struct ConvertRegsAarch64;
impl ConvertRegs for ConvertRegsAarch64 {
    type UnwindRegs = UnwindRegsAarch64;
    fn convert_regs(regs: &Regs) -> (u64, u64, UnwindRegsAarch64) {
        let ip = regs.get(PERF_REG_ARM64_PC).unwrap();
        let lr = regs.get(PERF_REG_ARM64_LR).unwrap();
        let sp = regs.get(PERF_REG_ARM64_SP).unwrap();
        let fp = regs.get(PERF_REG_ARM64_X29).unwrap();
        let regs = UnwindRegsAarch64::new(lr, sp, fp);
        (ip, sp, regs)
    }

    fn regs_mask() -> u64 {
        1 << PERF_REG_ARM64_PC
            | 1 << PERF_REG_ARM64_LR
            | 1 << PERF_REG_ARM64_SP
            | 1 << PERF_REG_ARM64_X29
    }
}

#[derive(Debug, Clone)]
pub struct EventInterpretation {
    pub main_event_attr_index: usize,
    #[allow(unused)]
    pub main_event_name: String,
    pub sampling_is_time_based: Option<u64>,
    pub have_context_switches: bool,
    pub sched_switch_attr_index: Option<usize>,
    pub rss_stat_attr_index: Option<usize>,
    pub event_names: Vec<String>,
}

impl EventInterpretation {
    pub fn divine_from_attrs(attrs: &[AttributeDescription]) -> Self {
        let main_event_attr_index = 0;
        let main_event_name = attrs[0]
            .name
            .as_deref()
            .unwrap_or("<unnamed event>")
            .to_string();
        let sampling_is_time_based = match (attrs[0].attr.type_, attrs[0].attr.sampling_policy) {
            (_, SamplingPolicy::NoSampling) => {
                panic!("Can only convert profiles with sampled events")
            }
            (_, SamplingPolicy::Frequency(freq)) => {
                let nanos = 1_000_000_000 / freq;
                Some(nanos)
            }
            (
                PerfEventType::Software(
                    SoftwareCounterType::CpuClock | SoftwareCounterType::TaskClock,
                ),
                SamplingPolicy::Period(period),
            ) => {
                // Assume that we're using a nanosecond clock. TODO: Check how we can know this for sure
                let nanos = u64::from(period);
                Some(nanos)
            }
            (_, SamplingPolicy::Period(_)) => None,
        };
        let have_context_switches = attrs[0].attr.flags.contains(AttrFlags::CONTEXT_SWITCH);
        let sched_switch_attr_index = attrs
            .iter()
            .position(|attr_desc| attr_desc.name.as_deref() == Some("sched:sched_switch"));
        let rss_stat_attr_index = attrs
            .iter()
            .position(|attr_desc| attr_desc.name.as_deref() == Some("kmem:rss_stat"));
        let event_names = attrs
            .iter()
            .enumerate()
            .map(|(attr_index, attr_desc)| {
                attr_desc
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("<unknown event {attr_index}>"))
            })
            .collect();

        Self {
            main_event_attr_index,
            main_event_name,
            sampling_is_time_based,
            have_context_switches,
            sched_switch_attr_index,
            rss_stat_attr_index,
            event_names,
        }
    }
}

pub type BoxedProductNameGenerator = Box<dyn FnOnce(&str) -> String>;

/// See [`Converter::check_for_pe_mapping`].
#[derive(Debug, Clone)]
struct SuspectedPeMapping {
    path: Vec<u8>,
    start: u64,
    size: u64,
}

pub struct Converter<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    cache: U::Cache,
    profile: Profile,
    processes: Processes<U>,
    timestamp_converter: TimestampConverter,
    current_sample_time: u64,
    build_ids: HashMap<DsoKey, DsoInfo>,
    endian: Endianness,
    have_product_name: bool,
    delayed_product_name_generator: Option<BoxedProductNameGenerator>,
    linux_version: Option<String>,
    extra_binary_artifact_dir: Option<PathBuf>,
    context_switch_handler: ContextSwitchHandler,
    unresolved_stacks: UnresolvedStacks,
    off_cpu_weight_per_sample: i32,
    have_context_switches: bool,
    event_names: Vec<String>,
    kernel_symbols: Option<KernelSymbols>,

    /// Mapping of start address to potential mapped PE binaries.
    /// The key is equal to the start field of the value.
    suspected_pe_mappings: BTreeMap<u64, SuspectedPeMapping>,

    jit_category_manager: JitCategoryManager,

    /// Whether a new thread should be merged into a previously exited
    /// thread of the same name.
    merge_threads: bool,

    /// Whether repeated frames at the base of the stack should be folded
    /// into one frame.
    fold_recursive_prefix: bool,
}

const DEFAULT_OFF_CPU_SAMPLING_INTERVAL_NS: u64 = 1_000_000; // 1ms

impl<U> Converter<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        product: &str,
        delayed_product_name_generator: Option<BoxedProductNameGenerator>,
        build_ids: HashMap<DsoKey, DsoInfo>,
        linux_version: Option<&str>,
        first_sample_time: u64,
        endian: Endianness,
        cache: U::Cache,
        extra_binary_artifact_dir: Option<&Path>,
        interpretation: EventInterpretation,
        merge_threads: bool,
        fold_recursive_prefix: bool,
    ) -> Self {
        let interval = match interpretation.sampling_is_time_based {
            Some(nanos) => SamplingInterval::from_nanos(nanos),
            None => SamplingInterval::from_millis(1),
        };
        let profile = Profile::new(
            product,
            ReferenceTimestamp::from_system_time(SystemTime::now()),
            interval,
        );
        let (off_cpu_sampling_interval_ns, off_cpu_weight_per_sample) =
            match &interpretation.sampling_is_time_based {
                Some(interval_ns) => (*interval_ns, 1),
                None => (DEFAULT_OFF_CPU_SAMPLING_INTERVAL_NS, 0),
            };
        let kernel_symbols = match KernelSymbols::new_for_running_kernel() {
            Ok(kernel_symbols) => Some(kernel_symbols),
            Err(err) => {
                eprintln!("Could not obtain kernel symbols: {err}");
                None
            }
        };
        Self {
            profile,
            cache,
            processes: Processes::new(merge_threads),
            timestamp_converter: TimestampConverter::with_reference_timestamp(first_sample_time),
            current_sample_time: first_sample_time,
            build_ids,
            endian,
            have_product_name: delayed_product_name_generator.is_none(),
            delayed_product_name_generator,
            linux_version: linux_version.map(ToOwned::to_owned),
            extra_binary_artifact_dir: extra_binary_artifact_dir.map(ToOwned::to_owned),
            off_cpu_weight_per_sample,
            context_switch_handler: ContextSwitchHandler::new(off_cpu_sampling_interval_ns),
            unresolved_stacks: UnresolvedStacks::default(),
            have_context_switches: interpretation.have_context_switches,
            event_names: interpretation.event_names,
            kernel_symbols,
            suspected_pe_mappings: BTreeMap::new(),
            jit_category_manager: JitCategoryManager::new(),
            merge_threads,
            fold_recursive_prefix,
        }
    }

    pub fn finish(mut self) -> Profile {
        let mut profile = self.profile;
        self.processes.finish(
            &mut profile,
            &self.unresolved_stacks,
            &self.event_names,
            &mut self.jit_category_manager,
            &self.timestamp_converter,
        );
        profile
    }

    pub fn handle_sample<C: ConvertRegs<UnwindRegs = U::UnwindRegs>>(&mut self, e: &SampleRecord) {
        let pid = e.pid.expect("Can't handle samples without pids");
        let tid = e.tid.expect("Can't handle samples without tids");
        let timestamp = e
            .timestamp
            .expect("Can't handle samples without timestamps");
        self.current_sample_time = timestamp;

        let profile_timestamp = self.timestamp_converter.convert_time(timestamp);

        let process = self.processes.get_by_pid(pid, &mut self.profile);
        process.check_jitdump(
            &mut self.jit_category_manager,
            &mut self.profile,
            &self.timestamp_converter,
        );

        let mut stack = Vec::new();
        Self::get_sample_stack::<C>(
            e,
            &process.unwinder,
            &mut self.cache,
            &mut stack,
            self.fold_recursive_prefix,
        );

        let thread = process.threads.get_thread_by_tid(tid, &mut self.profile);

        if thread.last_sample_timestamp == Some(timestamp) {
            // Duplicate sample. Ignore.
            return;
        }

        thread.last_sample_timestamp = Some(timestamp);
        let thread_handle = thread.profile_thread;

        // Consume off-cpu time and clear any saved off-CPU stack.
        let off_cpu_sample = self
            .context_switch_handler
            .handle_sample(timestamp, &mut thread.context_switch_data);
        if let (Some(off_cpu_sample), Some(off_cpu_stack)) =
            (off_cpu_sample, thread.off_cpu_stack.take())
        {
            let cpu_delta_ns = self
                .context_switch_handler
                .consume_cpu_delta(&mut thread.context_switch_data);
            process_off_cpu_sample_group(
                off_cpu_sample,
                thread_handle,
                cpu_delta_ns,
                &self.timestamp_converter,
                self.off_cpu_weight_per_sample,
                off_cpu_stack,
                &mut process.unresolved_samples,
            );
        }

        let cpu_delta = if self.have_context_switches {
            CpuDelta::from_nanos(
                self.context_switch_handler
                    .consume_cpu_delta(&mut thread.context_switch_data),
            )
        } else if let Some(period) = e.period {
            // If the observed perf event is one of the clock time events, or cycles, then we should convert it to a CpuDelta.
            // TODO: Detect event type
            CpuDelta::from_nanos(period)
        } else {
            CpuDelta::from_nanos(0)
        };

        let stack_index = self.unresolved_stacks.convert(stack.iter().rev().cloned());
        process.unresolved_samples.add_sample(
            thread_handle,
            profile_timestamp,
            timestamp,
            stack_index,
            cpu_delta,
            1,
        );
    }

    pub fn handle_sched_switch<C: ConvertRegs<UnwindRegs = U::UnwindRegs>>(
        &mut self,
        e: &SampleRecord,
    ) {
        let pid = e.pid.expect("Can't handle samples without pids");
        let tid = e.tid.expect("Can't handle samples without tids");
        let process = self.processes.get_by_pid(pid, &mut self.profile);
        process.check_jitdump(
            &mut self.jit_category_manager,
            &mut self.profile,
            &self.timestamp_converter,
        );

        let mut stack = Vec::new();
        Self::get_sample_stack::<C>(
            e,
            &process.unwinder,
            &mut self.cache,
            &mut stack,
            self.fold_recursive_prefix,
        );

        let stack_index = self
            .unresolved_stacks
            .convert_no_kernel(stack.iter().rev().cloned());
        let thread = process.threads.get_thread_by_tid(tid, &mut self.profile);
        thread.off_cpu_stack = Some(stack_index);
    }

    pub fn handle_rss_stat<C: ConvertRegs<UnwindRegs = U::UnwindRegs>>(
        &mut self,
        e: &SampleRecord,
    ) {
        let pid = e.pid.expect("Can't handle samples without pids");
        // let tid = e.tid.expect("Can't handle samples without tids");
        let process = self.processes.get_by_pid(pid, &mut self.profile);

        let Some(raw) = e.raw else { return };
        let Ok(rss_stat) = RssStat::parse(
            raw,
            self.endian,

        ) else { return };

        let Some(timestamp_mono) = e.timestamp else {
            eprintln!("rss_stat record doesn't have a timestamp");
            return;
        };
        let timestamp = self.timestamp_converter.convert_time(timestamp_mono);

        let (prev_size_of_this_member, member) = match rss_stat.member {
            MM_FILEPAGES => (
                &mut process.prev_mm_filepages_size,
                RssStatMember::ResidentFileMappingPages,
            ),
            MM_ANONPAGES => (
                &mut process.prev_mm_anonpages_size,
                RssStatMember::ResidentAnonymousPages,
            ),
            MM_SHMEMPAGES => (
                &mut process.prev_mm_shmempages_size,
                RssStatMember::ResidentSharedMemoryPages,
            ),
            MM_SWAPENTS => (
                &mut process.prev_mm_swapents_size,
                RssStatMember::AnonymousSwapEntries,
            ),
            _ => return,
        };

        let delta = rss_stat.size - *prev_size_of_this_member;
        *prev_size_of_this_member = rss_stat.size;

        if rss_stat.member == MM_ANONPAGES {
            let counter = process.get_or_make_mem_counter(&mut self.profile);
            self.profile
                .add_counter_sample(counter, timestamp, delta as f64, 1);
        }

        process.check_jitdump(
            &mut self.jit_category_manager,
            &mut self.profile,
            &self.timestamp_converter,
        );

        let mut stack = Vec::new();
        Self::get_sample_stack::<C>(
            e,
            &process.unwinder,
            &mut self.cache,
            &mut stack,
            self.fold_recursive_prefix,
        );
        let unresolved_stack = self.unresolved_stacks.convert(stack.into_iter().rev());
        let thread_handle = process.threads.main_thread.profile_thread;
        process.unresolved_samples.add_rss_stat_marker(
            thread_handle,
            timestamp,
            timestamp_mono,
            unresolved_stack,
            member,
            rss_stat.size,
            delta,
        );
    }

    pub fn handle_other_event_sample<C: ConvertRegs<UnwindRegs = U::UnwindRegs>>(
        &mut self,
        e: &SampleRecord,
        attr_index: usize,
    ) {
        let pid = e.pid.expect("Can't handle samples without pids");
        let timestamp_mono = e
            .timestamp
            .expect("Can't handle samples without timestamps");
        let timestamp = self.timestamp_converter.convert_time(timestamp_mono);
        // let tid = e.tid.expect("Can't handle samples without tids");
        let process = self.processes.get_by_pid(pid, &mut self.profile);
        process.check_jitdump(
            &mut self.jit_category_manager,
            &mut self.profile,
            &self.timestamp_converter,
        );

        let mut stack = Vec::new();
        Self::get_sample_stack::<C>(
            e,
            &process.unwinder,
            &mut self.cache,
            &mut stack,
            self.fold_recursive_prefix,
        );

        let thread_handle = match e.tid {
            Some(tid) => {
                process
                    .threads
                    .get_thread_by_tid(tid, &mut self.profile)
                    .profile_thread
            }
            None => process.threads.main_thread.profile_thread,
        };

        let unresolved_stack = self.unresolved_stacks.convert(stack.into_iter().rev());
        process.unresolved_samples.add_other_event_marker(
            thread_handle,
            timestamp,
            timestamp_mono,
            unresolved_stack,
            attr_index,
        );
    }

    /// Get the stack contained in this sample, and put it into `stack`.
    ///
    /// We can have both the kernel stack and the user stack, or just one of
    /// them, or neither. The stack is appended onto the `stack` outparameter,
    /// ordered from callee-most ("innermost") to caller-most. The kernel
    /// stack comes before the user stack.
    ///
    /// If the `SampleRecord` has a kernel stack, it's always in `e.callchain`.
    ///
    /// If this sample has a user stack, its source depends on the method of
    /// stackwalking that was requested during recording:
    ///
    ///  - With frame pointer unwinding (the default on x86, `perf record -g`,
    ///    or more explicitly `perf record --call-graph fp`), the user stack
    ///    is walked during sampling by the kernel and appended to e.callchain.
    ///  - With DWARF unwinding (`perf record --call-graph dwarf`), the raw
    ///    bytes on the stack are just copied into the perf.data file, and we
    ///    need to do the unwinding now, based on the register values in
    ///    `e.user_regs` and the raw stack bytes in `e.user_stack`.
    fn get_sample_stack<C: ConvertRegs<UnwindRegs = U::UnwindRegs>>(
        e: &SampleRecord,
        unwinder: &U,
        cache: &mut U::Cache,
        stack: &mut Vec<StackFrame>,
        fold_recursive_prefix: bool,
    ) {
        stack.truncate(0);

        // CpuMode::from_misc(e.raw.misc)

        // Get the first fragment of the stack from e.callchain.
        if let Some(callchain) = e.callchain {
            let mut is_first_frame = true;
            let mut mode = StackMode::from(e.cpu_mode);
            for i in 0..callchain.len() {
                let address = callchain.get(i).unwrap();
                if address >= PERF_CONTEXT_MAX {
                    if let Some(new_mode) = StackMode::from_context_frame(address) {
                        mode = new_mode;
                    }
                    continue;
                }

                let stack_frame = match is_first_frame {
                    true => StackFrame::InstructionPointer(address, mode),
                    false => StackFrame::ReturnAddress(address, mode),
                };
                stack.push(stack_frame);

                is_first_frame = false;
            }
        }

        // Append the user stack with the help of DWARF unwinding.
        if let (Some(regs), Some((user_stack, _))) = (&e.user_regs, e.user_stack) {
            let ustack_bytes = RawDataU64::from_raw_data::<LittleEndian>(user_stack);
            let (pc, sp, regs) = C::convert_regs(regs);
            let mut read_stack = |addr: u64| {
                // ustack_bytes has the stack bytes starting from the current stack pointer.
                let offset = addr.checked_sub(sp).ok_or(())?;
                let index = usize::try_from(offset / 8).map_err(|_| ())?;
                ustack_bytes.get(index).ok_or(())
            };

            // Unwind.
            let mut frames = unwinder.iter_frames(pc, regs, cache, &mut read_stack);
            loop {
                let frame = match frames.next() {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(_) => {
                        stack.push(StackFrame::TruncatedStackMarker);
                        break;
                    }
                };
                let stack_frame = match frame {
                    FrameAddress::InstructionPointer(addr) => {
                        StackFrame::InstructionPointer(addr, StackMode::User)
                    }
                    FrameAddress::ReturnAddress(addr) => {
                        StackFrame::ReturnAddress(addr.into(), StackMode::User)
                    }
                };
                stack.push(stack_frame);
            }
        }

        if stack.is_empty() {
            if let Some(ip) = e.ip {
                stack.push(StackFrame::InstructionPointer(ip, e.cpu_mode.into()));
            }
        } else if fold_recursive_prefix {
            let last_frame = *stack.last().unwrap();
            while stack.len() >= 2 && stack[stack.len() - 2] == last_frame {
                stack.pop();
            }
        }
    }

    /// This is a terrible hack to get binary correlation working with apps on Wine.
    ///
    /// Unlike ELF, PE has the notion of "file alignment" that is different from page alignment.
    /// Hence, even if the virtual address is page aligned, its on-disk offset may not be. This
    /// leads to obvious trouble with using mmap, since mmap requires the file offset to be page
    /// aligned. Wine's workaround is straightforward: for misaligned sections, Wine will simply
    /// copy the image from disk instead of mmapping them. For example, `/proc/<pid>/maps` can look
    /// like this:
    ///
    /// ```plain
    /// <PE header> 140000000-140001000 r--p 00000000 00:25 272185   game.exe
    /// <.text>     140001000-143be8000 r-xp 00000000 00:00 0
    ///             143be8000-144c0c000 r--p 00000000 00:00 0
    /// ```
    ///
    /// When this misalignment happens, most of the sections in the memory will not be a file
    /// mapping. However, the PE header is always mapped, and it resides at the beginning of the
    /// file, which means it's also always *aligned*. Finally, it's always mapped first, because
    /// the information from the header is required to determine the load address of the other
    /// sections. Hence, if we find a mapping that seems to pointing to a PE file, and has a file
    /// offset of 0, we'll add it to the list of "suspected PE images". When we see a later mapping
    /// that belongs to one of the suspected PE ranges, we'll match the mapping with the file,
    /// which allows binary correlation and unwinding to work.
    fn check_for_pe_mapping(&mut self, path_slice: &[u8], mapping_start_avma: u64) {
        // Do a quick extension check first, to avoid end up trying to parse every mmapped file.
        let filename_is_pe = path_slice.ends_with(b".exe")
            || path_slice.ends_with(b".dll")
            || path_slice.ends_with(b".EXE")
            || path_slice.ends_with(b".DLL");
        if !filename_is_pe {
            return;
        }

        // There are a few assumptions here:
        // - The SizeOfImage field in the PE header is defined to be a multiple of SectionAlignment.
        //   SectionAlignment is usually the page size. When it's not the page size, additional
        //   layout restrictions apply and Wine will always map the file in its entirety, which
        //   means we're safe without the workaround. So we can safely assume it to be page aligned
        //   here.
        // - VirtualAddress of the sections are defined to be adjacent after page-alignment. This
        //   means that we can treat the image as a contiguous region.
        if let Some(size) = get_pe_mapping_size(path_slice) {
            let mapping = SuspectedPeMapping {
                path: path_slice.to_owned(),
                start: mapping_start_avma,
                size,
            };
            self.suspected_pe_mappings.insert(mapping.start, mapping);
        }
    }

    pub fn handle_mmap(&mut self, e: MmapRecord, timestamp: u64) {
        let mut path = e.path.as_slice();
        if let Some(jitdump_path) = get_path_if_jitdump(&path) {
            let process = self.processes.get_by_pid(e.pid, &mut self.profile);
            process
                .jitdump_manager
                .add_jitdump_path(jitdump_path, self.extra_binary_artifact_dir.clone());
            return;
        }

        if e.page_offset == 0 {
            self.check_for_pe_mapping(&e.path.as_slice(), e.address);
        }

        if !e.is_executable {
            return;
        }

        let dso_key = match DsoKey::detect(&path, e.cpu_mode) {
            Some(dso_key) => dso_key,
            None => return,
        };
        let mut build_id = None;
        if let Some(dso_info) = self.build_ids.get(&dso_key) {
            build_id = Some(dso_info.build_id.to_owned());
            // Overwrite the path from the mmap record with the path from the build ID info.
            // These paths are usually the same, but in some cases the path from the build
            // ID info can be "better". For example, the synthesized mmap event for the
            // kernel vmlinux image usually has "[kernel.kallsyms]_text" whereas the build
            // ID info might have the full path to a kernel debug file, e.g.
            // "/usr/lib/debug/boot/vmlinux-4.16.0-1-amd64".
            path = dso_info.path.to_owned().into();
        }

        if e.pid == -1 {
            self.add_kernel_module(e.address, e.length, dso_key, build_id.as_deref(), &path);
        } else {
            self.add_module_to_process(
                e.pid,
                &path,
                e.page_offset,
                e.address,
                e.length,
                build_id.as_deref(),
                timestamp,
            );
        }
    }

    pub fn handle_mmap2(&mut self, e: Mmap2Record, timestamp: u64) {
        let path = e.path.as_slice();
        if let Some(jitdump_path) = get_path_if_jitdump(&path) {
            let process = self.processes.get_by_pid(e.pid, &mut self.profile);
            process
                .jitdump_manager
                .add_jitdump_path(jitdump_path, self.extra_binary_artifact_dir.clone());
            return;
        }

        if e.page_offset == 0 {
            self.check_for_pe_mapping(&e.path.as_slice(), e.address);
        }

        const PROT_EXEC: u32 = 0b100;
        if e.protection & PROT_EXEC == 0 {
            // Ignore non-executable mappings.
            return;
        }

        let build_id = match &e.file_id {
            Mmap2FileId::BuildId(build_id) => Some(build_id.to_owned()),
            Mmap2FileId::InodeAndVersion(_) => {
                let dso_key = match DsoKey::detect(&path, e.cpu_mode) {
                    Some(dso_key) => dso_key,
                    None => return,
                };
                self.build_ids
                    .get(&dso_key)
                    .map(|db| db.build_id.to_owned())
            }
        };

        self.add_module_to_process(
            e.pid,
            &path,
            e.page_offset,
            e.address,
            e.length,
            build_id.as_deref(),
            timestamp,
        );
    }

    pub fn handle_context_switch(&mut self, e: ContextSwitchRecord, common: CommonData) {
        let pid = common.pid.expect("Can't handle samples without pids");
        let tid = common.tid.expect("Can't handle samples without tids");
        let timestamp = common
            .timestamp
            .expect("Can't handle context switch without time");
        let process = self.processes.get_by_pid(pid, &mut self.profile);
        let thread = process.threads.get_thread_by_tid(tid, &mut self.profile);

        match e {
            ContextSwitchRecord::In { .. } => {
                // Consume off-cpu time and clear the saved off-CPU stack.
                let off_cpu_sample = self
                    .context_switch_handler
                    .handle_switch_in(timestamp, &mut thread.context_switch_data);
                if let (Some(off_cpu_sample), Some(off_cpu_stack)) =
                    (off_cpu_sample, thread.off_cpu_stack.take())
                {
                    let cpu_delta_ns = self
                        .context_switch_handler
                        .consume_cpu_delta(&mut thread.context_switch_data);
                    process_off_cpu_sample_group(
                        off_cpu_sample,
                        thread.profile_thread,
                        cpu_delta_ns,
                        &self.timestamp_converter,
                        self.off_cpu_weight_per_sample,
                        off_cpu_stack,
                        &mut process.unresolved_samples,
                    );
                }
            }
            ContextSwitchRecord::Out { .. } => {
                self.context_switch_handler
                    .handle_switch_out(timestamp, &mut thread.context_switch_data);
            }
        }
    }

    /// Called for a FORK record.
    ///
    /// FORK records are emitted if a new thread is started or if a new
    /// process is created. The name is inherited from the forking thread.
    pub fn handle_thread_start(&mut self, e: ForkOrExitRecord) {
        let start_time = self.timestamp_converter.convert_time(e.timestamp);

        let is_main = e.pid == e.tid;
        let parent_process = self.processes.get_by_pid(e.ppid, &mut self.profile);
        if e.pid != e.ppid {
            // We've created a new process.
            if !is_main {
                eprintln!("Unexpected data in FORK record: If we fork into a different process, the forked child thread should be the main thread of the new process");
            }
            let parent_process_name = parent_process.name.clone();
            let parent_thread = parent_process
                .threads
                .get_thread_by_tid(e.ptid, &mut self.profile);
            let parent_thread_name = parent_thread.name.clone();
            let is_reused = if let Some(name) = parent_process_name.as_deref() {
                self.processes.attempt_reuse(e.pid, name).is_some()
            } else {
                false
            };
            let process = self.processes.get_by_pid(e.pid, &mut self.profile);
            process.name = parent_process_name;
            let process_handle = process.profile_process;
            let thread = process.threads.get_main_thread();
            thread.name = parent_thread_name;
            let thread_handle = thread.profile_thread;
            if let Some(thread_name) = thread.name.as_deref() {
                self.profile.set_thread_name(thread_handle, thread_name);
            }
            if !is_reused {
                self.profile
                    .set_process_start_time(process_handle, start_time);
                self.profile
                    .set_thread_start_time(thread_handle, start_time);
            }
        } else {
            let parent_thread = parent_process
                .threads
                .get_thread_by_tid(e.ptid, &mut self.profile);
            let parent_thread_name = parent_thread.name.clone();
            let is_reused = if let Some(name) = parent_thread_name.as_deref() {
                parent_process
                    .threads
                    .attempt_thread_reuse(e.tid, name)
                    .is_some()
            } else {
                false
            };
            let mut thread = parent_process
                .threads
                .get_thread_by_tid(e.tid, &mut self.profile);
            thread.name = parent_thread_name;
            if !is_reused {
                let thread_handle = thread.profile_thread;
                if let Some(thread_name) = thread.name.as_deref() {
                    self.profile.set_thread_name(thread_handle, thread_name);
                }
                self.profile
                    .set_thread_start_time(thread_handle, start_time);
            }
        };
    }

    /// Called for an EXIT record.
    pub fn handle_thread_end(&mut self, e: ForkOrExitRecord) {
        let is_main = e.pid == e.tid;
        let end_time = self.timestamp_converter.convert_time(e.timestamp);
        if is_main {
            self.processes.remove(
                e.pid,
                end_time,
                &mut self.profile,
                &mut self.jit_category_manager,
                &self.timestamp_converter,
            );
        } else {
            let process = self.processes.get_by_pid(e.pid, &mut self.profile);
            process.threads.remove_non_main_thread(
                e.tid,
                end_time,
                self.merge_threads,
                &mut self.profile,
            );
        }
    }

    pub fn set_thread_name(&mut self, pid: i32, tid: i32, name: &str, is_thread_creation: bool) {
        let is_main = pid == tid;

        let process = self.processes.get_by_pid(pid, &mut self.profile);
        let process_handle = process.profile_process;

        let thread = process.threads.get_thread_by_tid(tid, &mut self.profile);
        let thread_handle = thread.profile_thread;

        self.profile.set_thread_name(thread_handle, name);
        thread.name = Some(name.to_owned());
        if is_main {
            self.profile.set_process_name(process_handle, name);
            process.name = Some(name.to_owned());
        }

        if is_thread_creation {
            // Mark this as the start time of the new thread / process.
            let time = self
                .timestamp_converter
                .convert_time(self.current_sample_time);
            self.profile.set_thread_start_time(thread_handle, time);
            if is_main {
                self.profile.set_process_start_time(process_handle, time);
            }
        }

        if self.delayed_product_name_generator.is_some() && name != "perf-exec" {
            let generator = self.delayed_product_name_generator.take().unwrap();
            let product = generator(name);
            self.profile.set_product(&product);
            self.have_product_name = true;
        }
    }

    pub fn handle_thread_name_update(&mut self, e: CommOrExecRecord, timestamp: Option<u64>) {
        let is_main = e.pid == e.tid;
        let name = e.name.as_slice();
        let name = String::from_utf8_lossy(&name);

        let is_thread_creation = if e.is_execve {
            // Mark the old thread / process as ended.
            // If the COMM record doesn't have a timestamp, take the last seen
            // timestamp from the previous sample.
            let timestamp = match timestamp {
                Some(0) | None => self.current_sample_time,
                Some(ts) => ts,
            };
            let end_time = self.timestamp_converter.convert_time(timestamp);
            if is_main {
                self.processes.remove(
                    e.pid,
                    end_time,
                    &mut self.profile,
                    &mut self.jit_category_manager,
                    &self.timestamp_converter,
                );
                let maybe_reused_process = self.processes.attempt_reuse(e.pid, &name);
                maybe_reused_process.is_none()
            } else {
                eprintln!(
                    "Unexpected is_execve on non-main thread! pid: {}, tid: {}",
                    e.pid, e.tid
                );
                let process = self.processes.get_by_pid(e.pid, &mut self.profile);
                process.threads.remove_non_main_thread(
                    e.tid,
                    end_time,
                    self.merge_threads,
                    &mut self.profile,
                );
                let maybe_reused_thread = process.threads.attempt_thread_reuse(e.tid, &name);
                maybe_reused_thread.is_none()
            }
        } else if self.merge_threads && !is_main {
            // Mark the old thread / process as ended.
            // If the COMM record doesn't have a timestamp, take the last seen
            // timestamp from the previous sample.
            let timestamp = match timestamp {
                Some(0) | None => self.current_sample_time,
                Some(ts) => ts,
            };
            let end_time = self.timestamp_converter.convert_time(timestamp);
            let process = self.processes.get_by_pid(e.pid, &mut self.profile);
            process.threads.remove_non_main_thread(
                e.tid,
                end_time,
                self.merge_threads,
                &mut self.profile,
            );
            let maybe_reused_thread = process.threads.attempt_thread_reuse(e.tid, &name);
            maybe_reused_thread.is_none()
        } else {
            false
        };

        self.set_thread_name(e.pid, e.tid, &name, is_thread_creation);
    }

    fn add_kernel_module(
        &mut self,
        base_address: u64,
        len: u64,
        dso_key: DsoKey,
        build_id: Option<&[u8]>,
        path: &[u8],
    ) {
        let path = std::str::from_utf8(path).unwrap().to_string();
        let build_id: Option<Vec<u8>> = match (build_id, self.kernel_symbols.as_ref()) {
            (None, Some(kernel_symbols)) if kernel_symbols.base_avma == base_address => {
                Some(kernel_symbols.build_id.clone())
            }
            (None, _) => {
                kernel_module_build_id(Path::new(&path), self.extra_binary_artifact_dir.as_deref())
            }
            (Some(build_id), _) => Some(build_id.to_owned()),
        };
        let debug_id = build_id
            .as_deref()
            .map(|id| DebugId::from_identifier(id, self.endian == Endianness::LittleEndian));

        let debug_path = match self.linux_version.as_deref() {
            Some(linux_version) if path.starts_with("[kernel.kallsyms]") => {
                // Take a guess at the vmlinux debug file path.
                format!("/usr/lib/debug/boot/vmlinux-{linux_version}")
            }
            _ => path.clone(),
        };
        let symbol_table = match (&dso_key, &build_id, self.kernel_symbols.as_ref()) {
            (DsoKey::Kernel, Some(build_id), Some(kernel_symbols))
                if build_id == &kernel_symbols.build_id && kernel_symbols.base_avma != 0 =>
            {
                // Run `echo '0' | sudo tee /proc/sys/kernel/kptr_restrict` to get here without root.
                Some(kernel_symbols.symbol_table.clone())
            }
            _ => None,
        };

        let lib_handle = self.profile.add_lib(LibraryInfo {
            debug_id: debug_id.unwrap_or_default(),
            path,
            debug_path,
            code_id: build_id.map(|build_id| CodeId::from_binary(&build_id).to_string()),
            name: dso_key.name().to_string(),
            debug_name: dso_key.name().to_string(),
            arch: None,
            symbol_table,
        });
        self.profile
            .add_kernel_lib_mapping(lib_handle, base_address, base_address + len, 0);
    }

    /// Tell the unwinder about this module, and alsos create a ProfileModule
    /// and add it to the profile.
    ///
    /// The unwinder needs to know about it in case we need to do DWARF stack
    /// unwinding - it needs to get the unwinding information from the binary.
    /// The profile needs to know about this module so that it can assign
    /// addresses in the stack to the right module and so that symbolication
    /// knows where to get symbols for this module.
    #[allow(clippy::too_many_arguments)]
    fn add_module_to_process(
        &mut self,
        process_pid: i32,
        path_slice: &[u8],
        mapping_start_file_offset: u64,
        mapping_start_avma: u64,
        mapping_size: u64,
        build_id: Option<&[u8]>,
        timestamp: u64,
    ) {
        let process = self.processes.get_by_pid(process_pid, &mut self.profile);

        let path = std::str::from_utf8(path_slice).unwrap();
        let (mut file, mut path): (Option<_>, String) = match open_file_with_fallback(
            Path::new(path),
            self.extra_binary_artifact_dir.as_deref(),
        ) {
            Ok((file, path)) => (Some(file), path.to_string_lossy().to_string()),
            _ => (None, path.to_owned()),
        };

        let mut suspected_pe_mapping = None;
        if file.is_none() {
            suspected_pe_mapping = self
                .suspected_pe_mappings
                .range(..=mapping_start_avma)
                .next_back()
                .map(|(_, m)| m)
                .filter(|m| {
                    mapping_start_avma >= m.start
                        && mapping_start_avma + mapping_size <= m.start + m.size
                });
            if let Some(mapping) = suspected_pe_mapping {
                if let Ok((pe_file, pe_path)) = open_file_with_fallback(
                    Path::new(std::str::from_utf8(&mapping.path).unwrap()),
                    self.extra_binary_artifact_dir.as_deref(),
                ) {
                    file = Some(pe_file);
                    path = pe_path.to_string_lossy().to_string();
                }
            }
        }

        if file.is_none() && !path.starts_with('[') {
            // eprintln!("Could not open file {:?}", objpath);
        }

        // Fix up bad files from `perf inject --jit`.
        if let Some(file_inner) = &file {
            if let Some((fixed_file, fixed_path)) = correct_bad_perf_jit_so_file(file_inner, &path)
            {
                file = Some(fixed_file);
                path = fixed_path;
            }
        }

        let mapping_end_avma = mapping_start_avma + mapping_size;
        let avma_range = mapping_start_avma..mapping_end_avma;

        let name = Path::new(&path)
            .file_name()
            .map_or("<unknown>".into(), |f| f.to_string_lossy().to_string());

        if let Some(file) = file {
            let mmap = match unsafe { memmap2::MmapOptions::new().map(&file) } {
                Ok(mmap) => mmap,
                Err(err) => {
                    eprintln!("Could not mmap file {path}: {err:?}");
                    return;
                }
            };

            fn section_data<'a>(section: &impl ObjectSection<'a>) -> Option<Vec<u8>> {
                section.uncompressed_data().ok().map(|data| data.to_vec())
            }

            let file = match object::File::parse(&mmap[..]) {
                Ok(file) => file,
                Err(_) => {
                    eprintln!("File {path} has unrecognized format");
                    return;
                }
            };

            // Verify build ID.
            if let Some(build_id) = build_id {
                match file.build_id().ok().flatten() {
                    Some(file_build_id) if build_id == file_build_id => {
                        // Build IDs match. Good.
                    }
                    Some(file_build_id) => {
                        let file_build_id = CodeId::from_binary(file_build_id);
                        let expected_build_id = CodeId::from_binary(build_id);
                        eprintln!(
                            "File {path} has non-matching build ID {file_build_id} (expected {expected_build_id})"
                        );
                        return;
                    }
                    None => {
                        eprintln!(
                            "File {path} does not contain a build ID, but we expected it to have one"
                        );
                        return;
                    }
                }
            }

            let base_svma = samply_symbols::relative_address_base(&file);
            let base_avma = if let Some(mapping) = suspected_pe_mapping {
                // For the PE correlation hack, we can't use the mapping offsets as they correspond to
                // an anonymous mapping. Instead, the base address is pre-determined from the PE header
                // mapping.
                mapping.start
            } else if let Some(bias) = compute_vma_bias(
                &file,
                mapping_start_file_offset,
                mapping_start_avma,
                mapping_size,
            ) {
                base_svma.wrapping_add(bias)
            } else {
                return;
            };

            let text = file.section_by_name(".text");
            let text_env = file.section_by_name("text_env");
            let eh_frame = file.section_by_name(".eh_frame");
            let got = file.section_by_name(".got");
            let eh_frame_hdr = file.section_by_name(".eh_frame_hdr");

            let unwind_data = match (
                eh_frame.as_ref().and_then(section_data),
                eh_frame_hdr.as_ref().and_then(section_data),
            ) {
                (Some(eh_frame), Some(eh_frame_hdr)) => {
                    ModuleUnwindData::EhFrameHdrAndEhFrame(eh_frame_hdr, eh_frame)
                }
                (Some(eh_frame), None) => ModuleUnwindData::EhFrame(eh_frame),
                (None, _) => ModuleUnwindData::None,
            };

            let text_data = if let Some(text_segment) = file
                .segments()
                .find(|segment| segment.name_bytes() == Ok(Some(b"__TEXT")))
            {
                let (start, size) = text_segment.file_range();
                let address_range = base_avma + start..base_avma + start + size;
                text_segment
                    .data()
                    .ok()
                    .map(|data| TextByteData::new(data.to_owned(), address_range))
            } else if let Some(text_section) = &text {
                if let Some((start, size)) = text_section.file_range() {
                    let address_range = base_avma + start..base_avma + start + size;
                    text_section
                        .data()
                        .ok()
                        .map(|data| TextByteData::new(data.to_owned(), address_range))
                } else {
                    None
                }
            } else {
                None
            };

            fn svma_range<'a>(section: &impl ObjectSection<'a>) -> Range<u64> {
                section.address()..section.address() + section.size()
            }

            let module = Module::new(
                path.to_string(),
                avma_range.clone(),
                base_avma,
                ModuleSvmaInfo {
                    base_svma,
                    text: text.as_ref().map(svma_range),
                    text_env: text_env.as_ref().map(svma_range),
                    stubs: None,
                    stub_helper: None,
                    eh_frame: eh_frame.as_ref().map(svma_range),
                    eh_frame_hdr: eh_frame_hdr.as_ref().map(svma_range),
                    got: got.as_ref().map(svma_range),
                },
                unwind_data,
                text_data,
            );
            process.unwinder.add_module(module);

            let debug_id = if let Some(debug_id) = debug_id_for_object(&file) {
                debug_id
            } else {
                return;
            };
            let code_id = file
                .build_id()
                .ok()
                .flatten()
                .map(|build_id| CodeId::from_binary(build_id).to_string());
            let lib_handle = self.profile.add_lib(LibraryInfo {
                debug_id,
                code_id,
                path: path.clone(),
                debug_path: path,
                debug_name: name.clone(),
                name: name.clone(),
                arch: None,
                symbol_table: None,
            });

            let relative_address_at_start = (avma_range.start - base_avma) as u32;

            if name.starts_with("jitted-") && name.ends_with(".so") {
                let symbol_name = jit_function_name(&file);
                process.add_lib_mapping_for_injected_jit_lib(
                    timestamp,
                    self.timestamp_converter.convert_time(timestamp),
                    symbol_name,
                    mapping_start_avma,
                    mapping_end_avma,
                    relative_address_at_start,
                    lib_handle,
                    &mut self.jit_category_manager,
                    &mut self.profile,
                );
            } else {
                process.add_regular_lib_mapping(
                    timestamp,
                    mapping_start_avma,
                    mapping_end_avma,
                    relative_address_at_start,
                    lib_handle,
                );
            }
        } else {
            // Without access to the binary file, make some guesses. We can't really
            // know what the right base address is because we don't have the section
            // information which lets us map between addresses and file offsets, but
            // often svmas and file offsets are the same, so this is a reasonable guess.
            let base_avma = mapping_start_avma - mapping_start_file_offset;
            let relative_address_at_start = (mapping_start_avma - base_avma) as u32;

            // If we have a build ID, convert it to a debug_id and a code_id.
            let debug_id = build_id
                .map(|id| DebugId::from_identifier(id, true)) // TODO: endian
                .unwrap_or_default();
            let code_id = build_id.map(|build_id| CodeId::from_binary(build_id).to_string());

            let lib_handle = self.profile.add_lib(LibraryInfo {
                debug_id,
                code_id,
                path: path.clone(),
                debug_path: path,
                debug_name: name.clone(),
                name,
                arch: None,
                symbol_table: None,
            });
            process.add_regular_lib_mapping(
                timestamp,
                mapping_start_avma,
                mapping_end_avma,
                relative_address_at_start,
                lib_handle,
            );
        }
    }
}

fn jit_function_name<'data>(obj: &object::File<'data>) -> Option<&'data str> {
    let mut text_symbols = obj.symbols().filter(|s| s.kind() == SymbolKind::Text);
    let symbol = text_symbols.next()?;
    symbol.name().ok()
}

// #[test]
// fn test_my_jit() {
//     let data = std::fs::read("/Users/mstange/Downloads/jitted-123175-0-fixed.so").unwrap();
//     let file = object::File::parse(&data[..]).unwrap();
//     dbg!(jit_function_name(&file));
// }

fn process_off_cpu_sample_group(
    off_cpu_sample: OffCpuSampleGroup,
    thread_handle: ThreadHandle,
    cpu_delta_ns: u64,
    timestamp_converter: &TimestampConverter,
    off_cpu_weight_per_sample: i32,
    off_cpu_stack: UnresolvedStackHandle,
    samples: &mut UnresolvedSamples,
) {
    let OffCpuSampleGroup {
        begin_timestamp,
        end_timestamp,
        sample_count,
    } = off_cpu_sample;

    // Add a sample at the beginning of the paused range.
    // This "first sample" will carry any leftover accumulated running time ("cpu delta").
    let cpu_delta = CpuDelta::from_nanos(cpu_delta_ns);
    let weight = off_cpu_weight_per_sample;
    let stack = off_cpu_stack;
    let profile_timestamp = timestamp_converter.convert_time(begin_timestamp);
    samples.add_sample(
        thread_handle,
        profile_timestamp,
        begin_timestamp,
        stack,
        cpu_delta,
        weight,
    );

    if sample_count > 1 {
        // Emit a "rest sample" with a CPU delta of zero covering the rest of the paused range.
        let cpu_delta = CpuDelta::from_nanos(0);
        let weight = i32::try_from(sample_count - 1).unwrap_or(0) * off_cpu_weight_per_sample;
        let profile_timestamp = timestamp_converter.convert_time(end_timestamp);
        samples.add_sample(
            thread_handle,
            profile_timestamp,
            begin_timestamp,
            stack,
            cpu_delta,
            weight,
        );
    }
}

struct Processes<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    processes_by_pid: HashMap<i32, Process<U>>,
    ended_processes_for_reuse_by_name: HashMap<String, VecDeque<Process<U>>>,

    /// The sample data for all removed processes.
    process_sample_datas: Vec<ProcessSampleData>,

    allow_reuse: bool,
}

impl<U> Processes<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    pub fn new(allow_reuse: bool) -> Self {
        Self {
            processes_by_pid: HashMap::new(),
            ended_processes_for_reuse_by_name: HashMap::new(),
            process_sample_datas: Vec::new(),
            allow_reuse,
        }
    }

    pub fn attempt_reuse(&mut self, pid: i32, name: &str) -> Option<&mut Process<U>> {
        if let Entry::Vacant(entry) = self.processes_by_pid.entry(pid) {
            if let Some(processes_of_same_name) =
                self.ended_processes_for_reuse_by_name.get_mut(name)
            {
                let mut process = processes_of_same_name
                    .pop_front()
                    .expect("We only have non-empty VecDeques in this HashMap");
                if processes_of_same_name.is_empty() {
                    self.ended_processes_for_reuse_by_name.remove(name);
                }
                process.reset_for_reuse(pid);
                return Some(entry.insert(process));
            }
        }
        None
    }

    pub fn get_by_pid(&mut self, pid: i32, profile: &mut Profile) -> &mut Process<U> {
        self.processes_by_pid.entry(pid).or_insert_with(|| {
            let name = format!("<{pid}>");
            let handle = profile.add_process(
                &name,
                pid as u32,
                Timestamp::from_millis_since_reference(0.0),
            );
            let profile_thread = profile.add_thread(
                handle,
                pid as u32,
                Timestamp::from_millis_since_reference(0.0),
                true,
            );
            let main_thread = Thread {
                profile_thread,
                context_switch_data: Default::default(),
                last_sample_timestamp: None,
                off_cpu_stack: None,
                name: None,
            };
            let jit_function_recycler = if self.allow_reuse {
                Some(JitFunctionRecycler::default())
            } else {
                None
            };
            Process {
                profile_process: handle,
                unwinder: U::default(),
                jitdump_manager: JitDumpManager::new_for_process(profile_thread),
                lib_mapping_ops: Default::default(),
                name: None,
                pid,
                threads: ProcessThreads {
                    pid,
                    profile_process: handle,
                    main_thread,
                    threads_by_tid: HashMap::new(),
                    ended_threads_for_reuse_by_name: HashMap::new(),
                },
                jit_function_recycler,
                unresolved_samples: Default::default(),
                prev_mm_filepages_size: 0,
                prev_mm_anonpages_size: 0,
                prev_mm_swapents_size: 0,
                prev_mm_shmempages_size: 0,
                mem_counter: None,
            }
        })
    }

    pub fn remove(
        &mut self,
        pid: i32,
        time: Timestamp,
        profile: &mut Profile,
        jit_category_manager: &mut JitCategoryManager,
        timestamp_converter: &TimestampConverter,
    ) {
        let Some(mut process) = self.processes_by_pid.remove(&pid) else { return };
        profile.set_process_end_time(process.profile_process, time);

        let process_sample_data = process.on_remove(
            self.allow_reuse,
            profile,
            jit_category_manager,
            timestamp_converter,
        );
        if !process_sample_data.is_empty() {
            self.process_sample_datas.push(process_sample_data);
        }

        if self.allow_reuse {
            if let Some(name) = process.name.as_deref() {
                self.ended_processes_for_reuse_by_name
                    .entry(name.to_string())
                    .or_default()
                    .push_back(process);
            }
        }
    }

    pub fn finish(
        mut self,
        profile: &mut Profile,
        unresolved_stacks: &UnresolvedStacks,
        event_names: &[String],
        jit_category_manager: &mut JitCategoryManager,
        timestamp_converter: &TimestampConverter,
    ) {
        // Gather the ProcessSampleData from any processes which are still alive at the end of profiling.
        for mut process in self.processes_by_pid.into_values() {
            let process_sample_data = process.on_remove(
                self.allow_reuse,
                profile,
                jit_category_manager,
                timestamp_converter,
            );
            if !process_sample_data.is_empty() {
                self.process_sample_datas.push(process_sample_data);
            }
        }

        let user_category = profile.add_category("User", CategoryColor::Yellow).into();
        let kernel_category = profile.add_category("Kernel", CategoryColor::Orange).into();
        let mut stack_frame_scratch_buf = Vec::new();
        for process_sample_data in self.process_sample_datas {
            process_sample_data.flush_samples_to_profile(
                profile,
                user_category,
                kernel_category,
                &mut stack_frame_scratch_buf,
                unresolved_stacks,
                event_names,
            );
        }
    }
}

#[derive(Debug)]
struct Thread {
    profile_thread: ThreadHandle,
    context_switch_data: ThreadContextSwitchData,
    last_sample_timestamp: Option<u64>,

    /// Some() between sched_switch and the next context switch IN
    ///
    /// Refers to a stack in the containing Process's UnresolvedSamples stack table.
    off_cpu_stack: Option<UnresolvedStackHandle>,
    name: Option<String>,
}

impl Thread {
    pub fn on_remove(&mut self) {
        self.context_switch_data = Default::default();
        self.last_sample_timestamp = None;
        self.off_cpu_stack = None;
    }

    pub fn reset_for_reuse(&mut self, _tid: i32) {}
}

struct Process<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    pub profile_process: ProcessHandle,
    pub unwinder: U,
    pub jitdump_manager: JitDumpManager,
    pub lib_mapping_ops: LibMappingOpQueue,
    pub name: Option<String>,
    pub threads: ProcessThreads,
    pid: i32,
    pub unresolved_samples: UnresolvedSamples,
    jit_function_recycler: Option<JitFunctionRecycler>,
    prev_mm_filepages_size: i64,
    prev_mm_anonpages_size: i64,
    prev_mm_swapents_size: i64,
    prev_mm_shmempages_size: i64,
    mem_counter: Option<CounterHandle>,
}

impl<U> Process<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    pub fn check_jitdump(
        &mut self,
        jit_category_manager: &mut JitCategoryManager,
        profile: &mut Profile,
        timestamp_converter: &TimestampConverter,
    ) {
        self.jitdump_manager.process_pending_records(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            timestamp_converter,
        );
    }

    pub fn reset_for_reuse(&mut self, new_pid: i32) {
        self.pid = new_pid;
        self.threads.pid = new_pid;
    }

    pub fn on_remove(
        &mut self,
        allow_thread_reuse: bool,
        profile: &mut Profile,
        jit_category_manager: &mut JitCategoryManager,
        timestamp_converter: &TimestampConverter,
    ) -> ProcessSampleData {
        self.unwinder = U::default();

        if allow_thread_reuse {
            self.threads.prepare_for_reuse();
        }

        let perf_map_mappings = if !self.unresolved_samples.is_empty() {
            try_load_perf_map(
                self.pid as u32,
                profile,
                jit_category_manager,
                self.jit_function_recycler.as_mut(),
            )
        } else {
            None
        };

        if let Some(recycler) = self.jit_function_recycler.as_mut() {
            recycler.finish_round();
        }

        let jitdump_manager = std::mem::replace(
            &mut self.jitdump_manager,
            JitDumpManager::new_for_process(self.threads.main_thread.profile_thread),
        );
        let jitdump_ops = jitdump_manager.finish(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            timestamp_converter,
        );

        ProcessSampleData::new(
            std::mem::take(&mut self.unresolved_samples),
            std::mem::take(&mut self.lib_mapping_ops),
            jitdump_ops,
            perf_map_mappings,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_regular_lib_mapping(
        &mut self,
        timestamp: u64,
        start_address: u64,
        end_address: u64,
        relative_address_at_start: u32,
        lib_handle: LibraryHandle,
    ) {
        self.lib_mapping_ops.push(
            timestamp,
            LibMappingOp::Add(LibMappingAdd {
                start_avma: start_address,
                end_avma: end_address,
                relative_address_at_start,
                info: LibMappingInfo::new_lib(lib_handle),
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_lib_mapping_for_injected_jit_lib(
        &mut self,
        timestamp: u64,
        profile_timestamp: Timestamp,
        symbol_name: Option<&str>,
        start_address: u64,
        end_address: u64,
        mut relative_address_at_start: u32,
        mut lib_handle: LibraryHandle,
        jit_category_manager: &mut JitCategoryManager,
        profile: &mut Profile,
    ) {
        let main_thread = self.threads.main_thread.profile_thread;
        let timing = MarkerTiming::Instant(profile_timestamp);
        profile.add_marker(
            main_thread,
            "JitFunctionAdd",
            JitFunctionAddMarker(symbol_name.unwrap_or("<unknown>").to_owned()),
            timing,
        );

        if let (Some(name), Some(recycler)) = (symbol_name, self.jit_function_recycler.as_mut()) {
            (lib_handle, relative_address_at_start) = recycler.recycle(
                start_address,
                end_address,
                relative_address_at_start,
                name,
                lib_handle,
            );
        }

        let (category, js_frame) =
            jit_category_manager.classify_jit_symbol(symbol_name.unwrap_or(""), profile);
        self.lib_mapping_ops.push(
            timestamp,
            LibMappingOp::Add(LibMappingAdd {
                start_avma: start_address,
                end_avma: end_address,
                relative_address_at_start,
                info: LibMappingInfo::new_jit_function(lib_handle, category, js_frame),
            }),
        );
    }

    pub fn get_or_make_mem_counter(&mut self, profile: &mut Profile) -> CounterHandle {
        *self.mem_counter.get_or_insert_with(|| {
            profile.add_counter(
                self.profile_process,
                "malloc",
                "Memory",
                "Amount of allocated memory",
            )
        })
    }
}

struct ProcessThreads {
    pid: i32,
    profile_process: ProcessHandle,
    main_thread: Thread,
    threads_by_tid: HashMap<i32, Thread>,
    ended_threads_for_reuse_by_name: HashMap<String, VecDeque<Thread>>,
}

impl ProcessThreads {
    pub fn prepare_for_reuse(&mut self) {
        for (_tid, mut thread) in self.threads_by_tid.drain() {
            thread.on_remove();

            if let Some(name) = thread.name.as_deref() {
                self.ended_threads_for_reuse_by_name
                    .entry(name.to_owned())
                    .or_default()
                    .push_back(thread);
            }
        }
    }

    pub fn attempt_thread_reuse(&mut self, tid: i32, name: &str) -> Option<&mut Thread> {
        if let Entry::Vacant(entry) = self.threads_by_tid.entry(tid) {
            if let Some(threads_of_same_name) = self.ended_threads_for_reuse_by_name.get_mut(name) {
                let mut thread = threads_of_same_name
                    .pop_front()
                    .expect("We only have non-empty VecDeques in this HashMap");
                if threads_of_same_name.is_empty() {
                    self.ended_threads_for_reuse_by_name.remove(name);
                }
                thread.reset_for_reuse(tid);
                return Some(entry.insert(thread));
            }
        }
        None
    }

    pub fn get_main_thread(&mut self) -> &mut Thread {
        &mut self.main_thread
    }

    pub fn get_thread_by_tid(&mut self, tid: i32, profile: &mut Profile) -> &mut Thread {
        if tid == self.pid {
            return &mut self.main_thread;
        }
        self.threads_by_tid.entry(tid).or_insert_with(|| {
            let profile_thread = profile.add_thread(
                self.profile_process,
                tid as u32,
                Timestamp::from_millis_since_reference(0.0),
                false,
            );
            Thread {
                profile_thread,
                context_switch_data: Default::default(),
                last_sample_timestamp: None,
                off_cpu_stack: None,
                name: None,
            }
        })
    }

    pub fn remove_non_main_thread(
        &mut self,
        tid: i32,
        time: Timestamp,
        allow_reuse: bool,
        profile: &mut Profile,
    ) {
        let Some(mut thread) = self.threads_by_tid.remove(&tid) else { return };
        profile.set_thread_end_time(thread.profile_thread, time);

        thread.on_remove();

        if allow_reuse {
            if let Some(name) = thread.name.as_deref() {
                self.ended_threads_for_reuse_by_name
                    .entry(name.to_owned())
                    .or_default()
                    .push_back(thread);
            }
        }
    }
}

// A file range in an object file, such as a segment or a section,
// for which we know the corresponding Stated Virtual Memory Address (SVMA).
#[derive(Clone)]
struct SvmaFileRange {
    svma: u64,
    file_offset: u64,
    size: u64,
}

impl SvmaFileRange {
    pub fn from_segment<'data, S: ObjectSegment<'data>>(segment: S) -> Self {
        let svma = segment.address();
        let (file_offset, size) = segment.file_range();
        SvmaFileRange {
            svma,
            file_offset,
            size,
        }
    }

    pub fn from_section<'data, S: ObjectSection<'data>>(section: S) -> Option<Self> {
        let svma = section.address();
        let (file_offset, size) = section.file_range()?;
        Some(SvmaFileRange {
            svma,
            file_offset,
            size,
        })
    }

    pub fn encompasses_file_range(&self, other_file_offset: u64, other_file_size: u64) -> bool {
        let self_file_range_end = self.file_offset + self.size;
        let other_file_range_end = other_file_offset + other_file_size;
        self.file_offset <= other_file_offset && other_file_range_end <= self_file_range_end
    }

    pub fn is_encompassed_by_file_range(
        &self,
        other_file_offset: u64,
        other_file_size: u64,
    ) -> bool {
        let self_file_range_end = self.file_offset + self.size;
        let other_file_range_end = other_file_offset + other_file_size;
        other_file_offset <= self.file_offset && self_file_range_end <= other_file_range_end
    }
}

impl Debug for SvmaFileRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SvmaFileRange")
            .field("svma", &format!("{:#x}", &self.svma))
            .field("file_offset", &format!("{:#x}", &self.file_offset))
            .field("size", &format!("{:#x}", &self.size))
            .finish()
    }
}

/// Compute the bias from the stated virtual memory address (SVMA), the VMA defined in the file's
/// section table, to the actual virtual memory address (AVMA), the VMA the file is actually mapped
/// at.
///
/// We have the section + segment information of the mapped object file, and we know the file offset
/// and size of the mapping, as well as the AVMA at the mapping start.
///
/// An image is mapped into memory using ELF load commands ("segments"). Usually there are multiple
/// ELF load commands, resulting in multiple mappings. Some of these mappings will be read-only,
/// and some will be executable.
///
/// Commonly, the executable mapping is the second of four mappings.
///
/// If we know about all mappings of an image, then the base AVMA is the start AVMA of the first mapping.
/// But sometimes we only have one mapping of an image, for example only the second mapping. This
/// happens if the `perf.data` file only contains mmap information of the executable mappings. In that
/// case, we have to look at the "page offset" of that mapping and find out which part of the file
/// was mapped in that range.
///
/// In that case it's tempting to say "The AVMA is the address at which the file would start, if
/// the entire file was mapped contiguously into memory around our mapping." While this works in
/// many cases, it doesn't work if there are "SVMA gaps" between the segments which have been elided
/// in the file, i.e. it doesn't work for files where the file offset <-> SVMA translation
/// is different for each segment.
///
/// Easy case A:
/// ```plain
/// File offset:  0x0 |----------------| |-------------|
/// SVMA:         0x0 |----------------| |-------------|
/// AVMA:   0x1750000 |----------------| |-------------|
/// ```
///
/// Easy case B:
/// ```plain
/// File offset:  0x0 |----------------| |-------------|
/// SVMA:     0x40000 |----------------| |-------------|
/// AVMA:   0x1750000 |----------------| |-------------|
/// ```
///
/// Hard case:
/// ```plain
/// File offset:  0x0 |----------------| |-------------|
/// SVMA:         0x0 |----------------|         |-------------|
/// AVMA:   0x1750000 |----------------|         |-------------|
/// ```
///
/// One example of the hard case has been observed in `libxul.so`: The `.text` section
/// was in the second segment. In the first segment, SVMAs were equal to their
/// corresponding file offsets. In the second segment, SMAs were 0x1000 bytes higher
/// than their corresponding file offsets. In other words, there was a 0x1000-wide gap
/// between the segments in virtual address space, but this gap was omitted in the file.
/// The SVMA gap exists in the AVMAs too - the "bias" between SVMAs and AVMAs is the same
/// for all segments of an image. So we have to find the SVMA for the mapping by finding
/// a segment or section which overlaps the mapping in file offset space, and then use
/// the matching segment's / section's SVMA to find the SVMA-to-AVMA "bias" for the
/// mapped bytes.
///
/// Another interesting edge case we observed was a case where a mapping was seemingly
/// not initiated by an ELF LOAD command: Part of the d8 binary (the V8 shell) was mapped
/// into memory with a mapping that covered only a small part of the `.text` section.
/// Usually, you'd expect a section to be mapped in its entirety, but this was not the
/// case here. So the segment finding code below checks for containment both ways: Whether
/// the mapping is contained in the segment, or whether the segment is contained in the
/// mapping. We also tried a solution where we just check for overlap between the segment
/// and the mapping, but this sometimes got the wrong segment, because the mapping is
/// larger than the segment due to alignment, and can extend into other segments.
fn compute_vma_bias<'data, 'file, O>(
    file: &'file O,
    mapping_start_file_offset: u64,
    mapping_start_avma: u64,
    mapping_size: u64,
) -> Option<u64>
where
    'data: 'file,
    O: Object<'data, 'file>,
{
    let mut contributions: Vec<SvmaFileRange> =
        file.segments().map(SvmaFileRange::from_segment).collect();

    if contributions.is_empty() {
        // If no segment is found, fall back to using section information.
        // This fallback only exists for the synthetic .so files created by `perf inject --jit`
        // - those don't have LOAD commands.
        contributions = file
            .sections()
            .filter(|s| s.kind() == SectionKind::Text)
            .filter_map(SvmaFileRange::from_section)
            .collect();
    }

    compute_vma_bias_impl(
        &contributions,
        mapping_start_file_offset,
        mapping_start_avma,
        mapping_size,
    )
}

fn compute_vma_bias_impl(
    contributions: &[SvmaFileRange],
    mapping_file_offset: u64,
    mapping_avma: u64,
    mapping_size: u64,
) -> Option<u64> {
    // Find a contribution which either fully contains the mapping, or which is fully contained by the mapping.
    // Linux perf simply always uses the .text section as the reference contribution.
    let ref_contribution = if let Some(contribution) = contributions.iter().find(|contribution| {
        contribution.encompasses_file_range(mapping_file_offset, mapping_size)
            || contribution.is_encompassed_by_file_range(mapping_file_offset, mapping_size)
    }) {
        contribution
    } else {
        println!(
            "Could not find segment or section overlapping the file offset range 0x{:x}..0x{:x}",
            mapping_file_offset,
            mapping_file_offset + mapping_size,
        );
        return None;
    };

    // Compute the AVMA at which the reference contribution is located in process memory.
    let ref_avma = if ref_contribution.file_offset > mapping_file_offset {
        mapping_avma + (ref_contribution.file_offset - mapping_file_offset)
    } else {
        mapping_avma - (mapping_file_offset - ref_contribution.file_offset)
    };

    // We have everything we need now.
    let bias = ref_avma.wrapping_sub(ref_contribution.svma);
    Some(bias)
}

#[test]
fn test_compute_base_avma_impl() {
    // From a local build of the Spidermonkey shell ("js")
    let js_segments = &[
        SvmaFileRange {
            svma: 0x0,
            file_offset: 0x0,
            size: 0x14bd0bc,
        },
        SvmaFileRange {
            svma: 0x14be0c0,
            file_offset: 0x14bd0c0,
            size: 0xf5bf60,
        },
        SvmaFileRange {
            svma: 0x241b020,
            file_offset: 0x2419020,
            size: 0x08e920,
        },
        SvmaFileRange {
            svma: 0x24aa940,
            file_offset: 0x24a7940,
            size: 0x002d48,
        },
    ];
    assert_eq!(
        compute_vma_bias_impl(js_segments, 0x14bd0c0, 0x100014be0c0, 0xf5bf60),
        Some(0x10000000000)
    );
    assert_eq!(
        compute_vma_bias_impl(js_segments, 0x14bd000, 0x55d605384000, 0xf5d000),
        Some(0x55d603ec6000)
    );

    // From a local build of the V8 shell ("d8")
    let d8_segments = &[
        SvmaFileRange {
            svma: 0x0,
            file_offset: 0x0,
            size: 0x3c8ed8,
        },
        SvmaFileRange {
            svma: 0x03ca000,
            file_offset: 0x3c9000,
            size: 0xfec770,
        },
        SvmaFileRange {
            svma: 0x13b7770,
            file_offset: 0x13b5770,
            size: 0x0528d0,
        },
        SvmaFileRange {
            svma: 0x140c000,
            file_offset: 0x1409000,
            size: 0x0118f0,
        },
    ];
    assert_eq!(
        compute_vma_bias_impl(d8_segments, 0x1056000, 0x55d15fe80000, 0x180000),
        Some(0x55d15ee29000)
    );
}

fn get_pe_mapping_size(path_slice: &[u8]) -> Option<u64> {
    fn inner<T: ImageNtHeaders>(data: &[u8]) -> Option<u64> {
        let file = PeFile::<T>::parse(data).ok()?;
        let size = file.nt_headers().optional_header().size_of_image();
        Some(size as u64)
    }

    let path = Path::new(std::str::from_utf8(path_slice).ok()?);
    let file = std::fs::File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };

    match FileKind::parse(&mmap[..]).ok()? {
        FileKind::Pe32 => inner::<ImageNtHeaders32>(&mmap),
        FileKind::Pe64 => inner::<ImageNtHeaders64>(&mmap),
        _ => None,
    }
}

fn kernel_module_build_id(
    path: &Path,
    extra_binary_artifact_dir: Option<&Path>,
) -> Option<Vec<u8>> {
    let file = open_file_with_fallback(path, extra_binary_artifact_dir)
        .ok()?
        .0;
    let mmap = unsafe { memmap2::MmapOptions::new().map(&file) }.ok()?;
    let obj = object::File::parse(&mmap[..]).ok()?;
    match obj.build_id() {
        Ok(Some(build_id)) => Some(build_id.to_owned()),
        _ => None,
    }
}

/// Correct unusable .so files from certain versions of perf which create ELF program
/// headers but fail to adjust addresses by the program header size.
///
/// For these bad files, we create a new, fixed, file, so that the mapping correctly
/// refers to the location of its .text section.
///
/// Background:
///
/// If you use `perf record` on applications which output JITDUMP information, such as
/// the V8 shell, you usually run `perf inject --jit` on your `perf.data` file afterwards.
/// This command interprets the JITDUMP file, and creates files of the form `jitted-12345-12.so`
/// which contain the JIT code from the JITDUMP file. There is one file per function.
/// These files are ELF files which look just enough like regular ELF files so that the
/// regular perf functionality works with them - unwinding, symbols, line numbers, assembly.
///
/// Before September 2022, these files did not contain any ELF program headers ("segments"),
/// they only contained sections. The files have 0x40 bytes of ELF header, followed by a
/// .text section at offset and address 0x40, followed by a few other sections, followed by
/// the section table.
///
/// This was changed by commit <https://github.com/torvalds/linux/commit/babd04386b1df8c364cdaa39ac0e54349502e1e5>,
/// "perf jit: Include program header in ELF files", in September 2022.
/// There was a bug in this commit, which was fixed by commit <https://github.com/torvalds/linux/commit/89b15d00527b7>,
/// "perf inject: Fix GEN_ELF_TEXT_OFFSET for jit", in October 2022.
///
/// Unfortunately, the first commit made it into a number of perf releases:
/// 4.19.215+, 5.4.215+, 5.10.145+, 5.15.71+, 5.19.12+, probably 6.0.16, and 6.1.2
///
/// The bug in the first commit means that, if you load the jit-ified perf.data file using
/// `samply load` and use the `jitted-12345-12.so` files as-is, the opened profile will
/// contain no useful information about JIT functions.
///
/// The broken files have a PT_LOAD command with file offset 0 and address 0, and a
/// .text section with file offset 0x80 and address 0x40. Furthermore, the synthesized
/// mmap record points at offset 0x40. The function name symbol is at address 0x40.
///
/// This means:
///  - The mapping does not encompass the entire .text section.
///  - We cannot calculate the image's base address in process memory because the .text
///    section has a different file-offset-to-address translation (-0x40) than the
///    PT_LOAD command (0x0). Neither of the translation amounts would give us a
///    base address that works correctly with the rest of the system: Following the
///    section might give us correct symbols but bad assembly, and the other way round.
///
/// We load these .so files twice: First, during profile conversion, and then later again
/// during symbolication and also when assembly code is looked up. We need to have a
/// consistent file on the file system which works with all these consumers.
///
/// So this function creates a fixed file and adjusts all fall paths to point to the fixed
/// file. The fixed file has its program header removed, so that the original symbol address
/// and mmap record are correct for the file offset and address 0x40.
/// We could also choose to keep the program header, but then we would need to adjust a
/// lot more: the mmap record, the symbol addresses, and the debug info.
fn correct_bad_perf_jit_so_file(
    file: &std::fs::File,
    path: &str,
) -> Option<(std::fs::File, String)> {
    if !path.contains("/jitted-") || !path.ends_with(".so") {
        return None;
    }

    let mmap = unsafe { memmap2::MmapOptions::new().map(file) }.ok()?;
    let obj = object::read::File::parse(&mmap[..]).ok()?;
    if obj.format() != object::BinaryFormat::Elf {
        return None;
    }

    // The bad files have exactly one segment, with offset 0x0 and address 0x0.
    let segment = obj.segments().next()?;
    if segment.address() != 0 || segment.file_range().0 != 0 {
        return None;
    }

    // The bad files have a .text section with offset 0x80 and address 0x40 (on x86_64).
    let text_section = obj.section_by_name(".text")?;
    if text_section.file_range()?.0 == text_section.address() {
        return None;
    }

    // All right, we have one of the broken files!

    // Let's make it right.
    let fixed_data = if obj.is_64() {
        object_rewriter::drop_phdr::<object::elf::FileHeader64<object::Endianness>>(&mmap[..])
            .ok()?
    } else {
        object_rewriter::drop_phdr::<object::elf::FileHeader32<object::Endianness>>(&mmap[..])
            .ok()?
    };
    let mut fixed_path = path.strip_suffix(".so").unwrap().to_string();
    fixed_path.push_str("-fixed.so");

    std::fs::write(&fixed_path, fixed_data).ok()?;

    // Open the fixed file for reading, and return it.
    let fixed_file = std::fs::File::open(&fixed_path).ok()?;

    Some((fixed_file, fixed_path))
}

/// Resident file mapping pages
#[allow(unused)]
const MM_FILEPAGES: i32 = 0;

/// Resident anonymous pages
#[allow(unused)]
const MM_ANONPAGES: i32 = 1;

/// Anonymous swap entries
#[allow(unused)]
const MM_SWAPENTS: i32 = 2;

/// Resident shared memory pages
#[allow(unused)]
const MM_SHMEMPAGES: i32 = 3;

/// ```
/// # cat /sys/kernel/debug/tracing/events/kmem/rss_stat/format
/// name: rss_stat
/// ID: 537
/// format:
///         field:unsigned short common_type;       offset:0;       size:2; signed:0;
///         field:unsigned char common_flags;       offset:2;       size:1; signed:0;
///         field:unsigned char common_preempt_count;       offset:3;       size:1; signed:0;
///         field:int common_pid;   offset:4;       size:4; signed:1;
///
///         field:unsigned int mm_id;       offset:8;       size:4; signed:0;
///         field:unsigned int curr;        offset:12;      size:4; signed:0;
///         field:int member;       offset:16;      size:4; signed:1;
///         field:long size;        offset:24;      size:8; signed:1;
///
/// print fmt: "mm_id=%u curr=%d type=%s size=%ldB", REC->mm_id, REC->curr, __print_symbolic(REC->member, { 0, "MM_FILEPAGES" }, { 1, "MM_ANONPAGES" }, { 2, "MM_SWAPENTS" }, { 3, "MM_SHMEMPAGES" }), REC->size
/// ```
#[repr(C)]
#[derive(Debug)]
struct RssStat {
    common_type: u16,
    common_flags: u8,
    common_preempt_count: u8,
    common_pid: i32,
    mm_id: u32,
    curr: u32,
    member: i32,
    size: i64,
}

impl RssStat {
    pub fn parse(data: RawData, endian: Endianness) -> Result<Self, std::io::Error> {
        match endian {
            Endianness::LittleEndian => Self::parse_impl::<byteorder::LittleEndian>(data),
            Endianness::BigEndian => Self::parse_impl::<byteorder::BigEndian>(data),
        }
    }

    pub fn parse_impl<O: ByteOrder>(mut data: RawData) -> Result<Self, std::io::Error> {
        let common_type = data.read_u16::<O>()?;
        let common_flags = data.read_u8()?;
        let common_preempt_count = data.read_u8()?;
        let common_pid = data.read_i32::<O>()?;
        let mm_id = data.read_u32::<O>()?;
        let curr = data.read_u32::<O>()?;
        let member = data.read_i32::<O>()?;
        let _padding = data.read_u32::<O>()?;
        let size = data.read_u64::<O>()? as i64;
        Ok(RssStat {
            common_type,
            common_flags,
            common_preempt_count,
            common_pid,
            mm_id,
            curr,
            member,
            size,
        })
    }
}

fn get_path_if_jitdump(path: &[u8]) -> Option<&Path> {
    let path = Path::new(std::str::from_utf8(path).ok()?);
    let filename = path.file_name()?.to_str()?;
    if filename.starts_with("jit-") && filename.ends_with(".dump") {
        Some(path)
    } else {
        None
    }
}
