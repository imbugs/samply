use std::collections::{HashMap, VecDeque};
use std::path::Path;

use debugid::DebugId;
use fxprof_processed_profile::{
    CategoryColor, CategoryHandle, CounterHandle, CpuDelta, Frame, FrameFlags, FrameInfo,
    LibraryInfo, Marker, MarkerFieldFormat, MarkerFieldSchema, MarkerHandle, MarkerLocation,
    MarkerSchema, MarkerTiming, ProcessHandle, Profile, SamplingInterval, StaticSchemaMarker,
    StringHandle, ThreadHandle, Timestamp,
};
use shlex::Shlex;
use wholesym::PeCodeId;

use super::chrome::KeywordNames;
use super::winutils;
use crate::shared::context_switch::{
    ContextSwitchHandler, OffCpuSampleGroup, ThreadContextSwitchData,
};
use crate::shared::included_processes::IncludedProcesses;
use crate::shared::jit_category_manager::JitCategoryManager;
use crate::shared::jit_function_add_marker::JitFunctionAddMarker;
use crate::shared::jit_function_recycler::JitFunctionRecycler;
use crate::shared::lib_mappings::{LibMappingAdd, LibMappingInfo, LibMappingOp, LibMappingOpQueue};
use crate::shared::process_name::make_process_name;
use crate::shared::process_sample_data::{ProcessSampleData, UserTimingMarker};
use crate::shared::recording_props::ProfileCreationProps;
use crate::shared::recycling::{ProcessRecycler, ProcessRecyclingData, ThreadRecycler};
use crate::shared::synthetic_jit_library::SyntheticJitLibrary;
use crate::shared::timestamp_converter::TimestampConverter;
use crate::shared::types::{StackFrame, StackMode};
use crate::shared::unresolved_samples::{UnresolvedSamples, UnresolvedStacks};
use crate::windows::firefox::{
    PHASE_INSTANT, PHASE_INTERVAL, PHASE_INTERVAL_END, PHASE_INTERVAL_START,
};

#[derive(Debug, Default, Clone)]
pub struct ImageInfoFromMergedEtl {
    pub image_timestamp: Option<u32>,
    pub debug_id: Option<DebugId>,
    pub pdb_path: Option<String>,
}

impl ImageInfoFromMergedEtl {
    pub fn get_complete(self) -> Option<(u32, DebugId, String)> {
        let image_timestamp = self.image_timestamp?;
        let debug_id = self.debug_id?;
        let pdb_path = self.pdb_path?;
        Some((image_timestamp, debug_id, pdb_path))
    }
}

/// An on- or off-cpu-sample for which the user stack is not known yet.
/// Consumed once the user stack arrives.
#[derive(Debug, Clone)]
pub struct PendingStack {
    /// The timestamp of the SampleProf or CSwitch event
    pub timestamp: u64,
    /// Starts out as None. Once we encounter the kernel stack (if any), we put it here.
    pub kernel_stack: Option<Vec<StackFrame>>,
    pub off_cpu_sample_group: Option<OffCpuSampleGroup>,
    pub on_cpu_sample_cpu_delta: Option<CpuDelta>,
}

#[derive(Debug)]
pub struct MemoryUsage {
    pub counter: CounterHandle,
    #[allow(dead_code)]
    pub value: f64,
}

#[derive(Debug)]
pub struct PendingMarker {
    pub text: String,
    pub start: Timestamp,
}

#[derive(Debug)]
pub struct ThreadState {
    pub name: Option<String>,
    #[allow(dead_code)]
    pub is_main_thread: bool,
    pub handle: ThreadHandle,
    pub label_frame: FrameInfo,
    pub pending_stacks: VecDeque<PendingStack>,
    pub context_switch_data: ThreadContextSwitchData,
    #[allow(dead_code)]
    pub thread_id: u32,
    #[allow(dead_code)]
    pub process_id: u32,
    pub pending_markers: HashMap<String, PendingMarker>,
}

impl ThreadState {
    fn new(
        name: Option<String>,
        is_main_thread: bool,
        handle: ThreadHandle,
        label_frame: FrameInfo,
        pid: u32,
        tid: u32,
    ) -> Self {
        ThreadState {
            name,
            is_main_thread,
            handle,
            label_frame,
            pending_stacks: VecDeque::new(),
            context_switch_data: Default::default(),
            pending_markers: HashMap::new(),
            thread_id: tid,
            process_id: pid,
        }
    }
}

pub struct ProcessState {
    pub name: String,
    pub handle: ProcessHandle,
    pub seen_main_thread_start: bool,
    pub unresolved_samples: UnresolvedSamples,
    pub regular_lib_mapping_ops: LibMappingOpQueue,
    pub jit_lib_mapping_ops: LibMappingOpQueue,
    pub main_thread_handle: ThreadHandle,
    pub main_thread_label_frame: FrameInfo,
    pub memory_usage: Option<MemoryUsage>,
    pub process_id: u32,
    #[allow(dead_code)]
    pub parent_id: u32,
    pub thread_recycler: Option<ThreadRecycler>,
    pub jit_function_recycler: Option<JitFunctionRecycler>,
}

impl ProcessState {
    pub fn take_recycling_data(&mut self) -> Option<ProcessRecyclingData> {
        let mut jit_function_recycler = self.jit_function_recycler.take()?;
        let thread_recycler = self.thread_recycler.take()?;

        jit_function_recycler.finish_round();

        Some(ProcessRecyclingData {
            process_handle: self.handle,
            main_thread_recycling_data: (
                self.main_thread_handle,
                self.main_thread_label_frame.clone(),
            ),
            thread_recycler,
            jit_function_recycler,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_jit_function(
        &mut self,
        timestamp_raw: u64,
        jit_lib: &mut SyntheticJitLibrary,
        name: String,
        start_avma: u64,
        size: u32,
        info: LibMappingInfo,
    ) {
        let relative_address = jit_lib.add_function(name, size);

        self.jit_lib_mapping_ops.push(
            timestamp_raw,
            LibMappingOp::Add(LibMappingAdd {
                start_avma,
                end_avma: start_avma + u64::from(size),
                relative_address_at_start: relative_address,
                info,
            }),
        );
    }
}

// Known profiler categories, lazy-created
#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
pub enum KnownCategory {
    Default,
    User,
    Kernel,
    System,
    D3DVideoSubmitDecoderBuffers,
    CoreClrR2r,
    CoreClrJit,
    CoreClrGc,
    Unknown,
}

struct KnownCategories(HashMap<KnownCategory, CategoryHandle>);

impl KnownCategories {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    #[rustfmt::skip]
    const CATEGORIES: &'static [(KnownCategory, &'static str, CategoryColor)] = &[
        (KnownCategory::User, "User", CategoryColor::Yellow),
        (KnownCategory::Kernel, "Kernel", CategoryColor::LightRed),
        (KnownCategory::System, "System Libraries", CategoryColor::Orange),
        (KnownCategory::D3DVideoSubmitDecoderBuffers, "D3D Video Submit Decoder Buffers", CategoryColor::Transparent),
        (KnownCategory::CoreClrR2r, "CoreCLR R2R", CategoryColor::Blue),
        (KnownCategory::CoreClrJit, "CoreCLR JIT", CategoryColor::Purple),
        (KnownCategory::CoreClrGc, "CoreCLR GC", CategoryColor::Red),
        (KnownCategory::Unknown, "Other", CategoryColor::DarkGray),
    ];

    pub fn get(&mut self, category: KnownCategory, profile: &mut Profile) -> CategoryHandle {
        let category = if category == KnownCategory::Default {
            KnownCategory::User
        } else {
            category
        };

        *self.0.entry(category).or_insert_with(|| {
            let (category_name, color) = Self::CATEGORIES
                .iter()
                .find(|(c, _, _)| *c == category)
                .map(|(_, name, color)| (*name, *color))
                .unwrap();
            profile.add_category(category_name, color)
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct AddressClassifier {
    kernel_min: u64,
}

impl AddressClassifier {
    pub fn get_stack_mode(&self, address: u64) -> StackMode {
        if address >= self.kernel_min {
            StackMode::Kernel
        } else {
            StackMode::User
        }
    }
}

pub struct ProfileContext {
    profile: Profile,

    profile_creation_props: ProfileCreationProps,

    // state -- keep track of the processes etc we've seen as we're processing,
    // and their associated handles in the json profile
    processes: HashMap<u32, ProcessState>,
    dead_processes_with_reused_pids: Vec<ProcessState>,
    threads: HashMap<u32, ThreadState>,
    dead_threads_with_reused_tids: Vec<ThreadState>,

    unresolved_stacks: UnresolvedStacks,

    /// Some() if a thread should be merged into a previously exited
    /// thread of the same name.
    process_recycler: Option<ProcessRecycler>,

    // some special threads
    gpu_thread_handle: Option<ThreadHandle>,

    // These are the processes + their descendants that we want to write into
    // the profile.json. If it's None, include everything.
    included_processes: Option<IncludedProcesses>,

    categories: KnownCategories,

    js_category_manager: JitCategoryManager,
    js_jit_lib: SyntheticJitLibrary,
    coreclr_jit_lib: SyntheticJitLibrary,

    context_switch_handler: ContextSwitchHandler,

    // cache of device mappings
    device_mappings: HashMap<String, String>, // map of \Device\HarddiskVolume4 -> C:\

    // the minimum address for kernel drivers, so that we can assign kernel_category to the frame
    kernel_min: u64,

    address_classifier: AddressClassifier,

    // architecture to record in the trace. will be the system architecture for now.
    // TODO no idea how to handle "I'm on aarch64 windows but I'm recording a win64 process".
    // I have no idea how stack traces work in that case anyway, so this is probably moot.
    arch: String,

    sample_count: usize,
    stack_sample_count: usize,
    event_count: usize,

    timestamp_converter: TimestampConverter,
    event_timestamps_are_qpc: bool,

    /// Only include main threads.
    main_thread_only: bool,

    // Time range from the timestamp origin
    time_range: Option<(Timestamp, Timestamp)>,
}

impl ProfileContext {
    pub fn new(
        mut profile: Profile,
        arch: &str,
        included_processes: Option<IncludedProcesses>,
        profile_creation_props: ProfileCreationProps,
    ) -> Self {
        // On 64-bit systems, the kernel address space always has 0xF in the first 16 bits.
        // The actual kernel address space is much higher, but we just need this to disambiguate kernel and user
        // stacks.
        let kernel_min: u64 = if arch == "x86" {
            0x8000_0000
        } else {
            0xF000_0000_0000_0000
        };
        let address_classifier = AddressClassifier { kernel_min };
        let process_recycler = if profile_creation_props.reuse_threads {
            Some(ProcessRecycler::new())
        } else {
            None
        };
        let main_thread_only = profile_creation_props.main_thread_only;
        let time_range = profile_creation_props.time_range.map(|(start, end)| {
            (
                Timestamp::from_nanos_since_reference(start.as_nanos() as u64),
                Timestamp::from_nanos_since_reference(end.as_nanos() as u64),
            )
        });

        let mut categories = KnownCategories::new();
        let mut js_category_manager = JitCategoryManager::new();
        let default_js_jit_category = js_category_manager.default_category(&mut profile);
        let allow_jit_function_recycling = profile_creation_props.reuse_threads;
        let js_jit_lib = SyntheticJitLibrary::new(
            "JS JIT".to_string(),
            default_js_jit_category.into(),
            &mut profile,
            allow_jit_function_recycling,
        );
        let coreclr_jit_category = categories.get(KnownCategory::CoreClrJit, &mut profile);
        let coreclr_jit_lib = SyntheticJitLibrary::new(
            "CoreCLR JIT".to_string(),
            coreclr_jit_category.into(),
            &mut profile,
            allow_jit_function_recycling,
        );

        Self {
            profile,
            profile_creation_props,
            processes: HashMap::new(),
            dead_processes_with_reused_pids: Vec::new(),
            threads: HashMap::new(),
            dead_threads_with_reused_tids: Vec::new(),
            unresolved_stacks: UnresolvedStacks::default(),
            process_recycler,
            gpu_thread_handle: None,
            included_processes,
            categories,
            js_category_manager,
            js_jit_lib,
            coreclr_jit_lib,
            context_switch_handler: ContextSwitchHandler::new(122100), // hardcoded, but replaced once TraceStart is received
            device_mappings: winutils::get_dos_device_mappings(),
            kernel_min,
            address_classifier,
            arch: arch.to_string(),
            sample_count: 0,
            stack_sample_count: 0,
            event_count: 0,
            // Dummy, will be replaced once we see the header
            timestamp_converter: TimestampConverter {
                reference_raw: 0,
                raw_to_ns_factor: 1,
            },
            event_timestamps_are_qpc: false,
            main_thread_only,
            time_range,
        }
    }

    pub fn creation_props(&self) -> ProfileCreationProps {
        self.profile_creation_props.clone()
    }

    pub fn is_arm64(&self) -> bool {
        self.arch == "arm64"
    }

    pub fn has_thread(&self, tid: u32) -> bool {
        self.threads.contains_key(&tid)
    }

    pub fn get_or_create_memory_usage_counter(&mut self, pid: u32) -> Option<CounterHandle> {
        let process = self.processes.get_mut(&pid)?;
        let process_handle = process.handle;
        let memory_usage = process.memory_usage.get_or_insert_with(|| {
            let counter = self.profile.add_counter(
                process_handle,
                "VM",
                "Memory",
                "Amount of VirtualAlloc allocated memory",
            );
            MemoryUsage {
                counter,
                value: 0.0,
            }
        });
        Some(memory_usage.counter)
    }

    pub fn is_interesting_process(&self, pid: u32, ppid: Option<u32>, name: Option<&str>) -> bool {
        if pid == 0 {
            return false;
        }

        // already tracking this process or its parent?
        if self.processes.contains_key(&pid)
            || ppid.is_some_and(|k| self.processes.contains_key(&k))
        {
            return true;
        }

        match &self.included_processes {
            Some(incl) => incl.should_include(name, pid),
            None => true,
        }
    }

    // The filename is a NT kernel path (https://chrisdenton.github.io/omnipath/NT.html) which isn't direclty
    // usable from user space.  perfview goes through a dance to convert it to a regular user space path
    // https://github.com/microsoft/perfview/blob/4fb9ec6947cb4e68ac7cb5e80f50ae3757d0ede4/src/TraceEvent/Parsers/KernelTraceEventParser.cs#L3461
    // and we do a bit of it here, just for dos drive mappings. Everything else we prefix with \\?\GLOBALROOT\
    pub fn map_device_path(&self, path: &str) -> String {
        for (k, v) in &self.device_mappings {
            if path.starts_with(k) {
                let r = format!("{}{}", v, path.split_at(k.len()).1);
                return r;
            }
        }

        // if we didn't translate (still have a \\ path), prefix with GLOBALROOT as
        // an escape
        if path.starts_with("\\\\") {
            format!("\\\\?\\GLOBALROOT{}", path)
        } else {
            path.into()
        }
    }

    pub fn known_category(&mut self, known_category: KnownCategory) -> CategoryHandle {
        self.categories.get(known_category, &mut self.profile)
    }

    pub fn intern_profile_string(&mut self, s: &str) -> StringHandle {
        self.profile.intern_string(s)
    }

    pub fn add_thread_instant_marker(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        marker: impl Marker,
    ) -> MarkerHandle {
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        let timing = MarkerTiming::Instant(timestamp);
        let thread = self.threads.get_mut(&tid).unwrap();
        self.profile.add_marker(thread.handle, timing, marker)
    }

    pub fn add_thread_interval_marker(
        &mut self,
        start_timestamp_raw: u64,
        end_timestamp_raw: u64,
        tid: u32,
        marker: impl Marker,
    ) -> MarkerHandle {
        let start_timestamp = self.timestamp_converter.convert_time(start_timestamp_raw);
        let end_timestamp = self.timestamp_converter.convert_time(end_timestamp_raw);
        let timing = MarkerTiming::Interval(start_timestamp, end_timestamp);
        let thread = self.threads.get(&tid).unwrap();
        self.profile.add_marker(thread.handle, timing, marker)
    }

    pub fn handle_header(&mut self, timestamp_raw: u64, perf_freq: u64, clock_type: u32) {
        if clock_type != 1 {
            log::warn!("QPC not used as clock");
            self.event_timestamps_are_qpc = false;
        } else {
            self.event_timestamps_are_qpc = true;
        }

        self.timestamp_converter = TimestampConverter {
            reference_raw: timestamp_raw,
            raw_to_ns_factor: 1000 * 1000 * 1000 / perf_freq,
        };
    }

    pub fn handle_collection_start(&mut self, interval_raw: u32) {
        let interval_nanos = interval_raw as u64 * 100;
        let interval = SamplingInterval::from_nanos(interval_nanos);
        log::info!("Sample rate {}ms", interval.as_secs_f64() * 1000.);
        self.profile.set_interval(interval);
        self.context_switch_handler = ContextSwitchHandler::new(interval_raw as u64);
    }

    pub fn make_process_name(&self, image_file_name: &str, cmdline: &str) -> String {
        let executable_path = self.map_device_path(image_file_name);
        let executable_name = extract_filename(&executable_path);
        make_process_name(
            executable_name,
            Shlex::new(cmdline).collect(),
            self.profile_creation_props
                .arg_count_to_include_in_process_name,
        )
    }

    pub fn handle_process_dcstart(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        parent_pid: u32,
        image_file_name: String,
        cmdline: String,
    ) {
        if !self.is_interesting_process(pid, Some(parent_pid), Some(&image_file_name)) {
            return;
        }

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        let name = self.make_process_name(&image_file_name, &cmdline);
        let process_handle = self.profile.add_process(&name, pid, timestamp);
        let main_thread_handle = self
            .profile
            .add_thread(process_handle, pid, timestamp, true);
        let main_thread_label_frame =
            make_thread_label_frame(&mut self.profile, Some(&name), pid, pid);
        let (thread_recycler, jit_function_recycler) = if self.process_recycler.is_some() {
            (
                Some(ThreadRecycler::new()),
                Some(JitFunctionRecycler::default()),
            )
        } else {
            (None, None)
        };
        let process = ProcessState {
            name,
            seen_main_thread_start: false,
            handle: process_handle,
            unresolved_samples: UnresolvedSamples::default(),
            regular_lib_mapping_ops: LibMappingOpQueue::default(),
            main_thread_handle,
            main_thread_label_frame,
            memory_usage: None,
            process_id: pid,
            parent_id: parent_pid,
            jit_lib_mapping_ops: LibMappingOpQueue::default(),
            thread_recycler,
            jit_function_recycler,
        };
        self.processes.insert(pid, process);
    }

    pub fn handle_process_start(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        parent_pid: u32,
        image_file_name: String,
        cmdline: String,
    ) {
        if !self.is_interesting_process(pid, Some(parent_pid), Some(&image_file_name)) {
            return;
        }

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);

        if let Some(dead_process_with_reused_pid) = self.processes.remove(&pid) {
            self.profile
                .set_process_end_time(dead_process_with_reused_pid.handle, timestamp);
            self.dead_processes_with_reused_pids
                .push(dead_process_with_reused_pid);
        }

        let name = self.make_process_name(&image_file_name, &cmdline);
        if let Some(process_recycler) = self.process_recycler.as_mut() {
            if let Some(ProcessRecyclingData {
                process_handle,
                main_thread_recycling_data,
                thread_recycler,
                jit_function_recycler,
            }) = process_recycler.recycle_by_name(&name)
            {
                log::info!("Found old process for pid {} and name {}", pid, name);
                let (main_thread_handle, main_thread_label_frame) = main_thread_recycling_data;
                let process = ProcessState {
                    name,
                    seen_main_thread_start: false,
                    handle: process_handle,
                    unresolved_samples: UnresolvedSamples::default(),
                    regular_lib_mapping_ops: LibMappingOpQueue::default(),
                    main_thread_handle,
                    main_thread_label_frame,
                    memory_usage: None,
                    process_id: pid,
                    parent_id: parent_pid,
                    jit_lib_mapping_ops: LibMappingOpQueue::default(),
                    thread_recycler: Some(thread_recycler),
                    jit_function_recycler: Some(jit_function_recycler),
                };
                self.processes.insert(pid, process);
                return;
            }
        }
        let process_handle = self.profile.add_process(&name, pid, timestamp);
        let main_thread_handle = self
            .profile
            .add_thread(process_handle, pid, timestamp, true);
        let main_thread_label_frame =
            make_thread_label_frame(&mut self.profile, Some(&name), pid, pid);
        let (thread_recycler, jit_function_recycler) = if self.process_recycler.is_some() {
            (
                Some(ThreadRecycler::new()),
                Some(JitFunctionRecycler::default()),
            )
        } else {
            (None, None)
        };
        let process = ProcessState {
            name,
            seen_main_thread_start: false,
            handle: process_handle,
            unresolved_samples: UnresolvedSamples::default(),
            regular_lib_mapping_ops: LibMappingOpQueue::default(),
            main_thread_handle,
            main_thread_label_frame,
            memory_usage: None,
            process_id: pid,
            parent_id: parent_pid,
            jit_lib_mapping_ops: LibMappingOpQueue::default(),
            thread_recycler,
            jit_function_recycler,
        };
        self.processes.insert(pid, process);
    }

    pub fn handle_process_end(&mut self, timestamp_raw: u64, pid: u32) {
        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        self.profile.set_process_end_time(process.handle, timestamp);

        if let Some(process_recycler) = self.process_recycler.as_mut() {
            if let Some(process_recycling_data) = process.take_recycling_data() {
                process_recycler.add_to_pool(&process.name, process_recycling_data);
                log::info!(
                    "Adding process with pid {} and name {} to pool",
                    process.process_id,
                    process.name
                );
            } else {
                log::info!("Could not get process recycling data");
            }
        }
    }

    pub fn handle_process_dcend(&mut self, _timestamp_raw: u64, _pid: u32) {
        // Nothing to do - the process is still alive at the end of profiling.
    }

    pub fn handle_thread_dcstart(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        pid: u32,
        name: Option<String>,
    ) {
        if !self.is_interesting_process(pid, None, None) {
            return;
        }

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        if !self.processes.contains_key(&pid) {
            log::warn!("Adding thread {tid} for unknown pid {pid}");
            return;
        }

        let process = self.processes.get_mut(&pid).unwrap();
        if !process.seen_main_thread_start {
            process.seen_main_thread_start = true;
            let thread_handle = process.main_thread_handle;
            let thread_label_frame =
                make_thread_label_frame(&mut self.profile, name.as_deref(), pid, tid);
            process.main_thread_label_frame = thread_label_frame.clone();
            // self.profile.set_thread_tid(thread_handle, tid);
            let thread = ThreadState::new(name, true, thread_handle, thread_label_frame, pid, tid);
            self.threads.insert(tid, thread);
            return;
        }

        if self.main_thread_only {
            // Ignore this thread.
            return;
        }

        let thread_handle = self
            .profile
            .add_thread(process.handle, tid, timestamp, false);
        let thread_label_frame =
            make_thread_label_frame(&mut self.profile, name.as_deref(), pid, tid);
        if let Some(name) = name.as_deref() {
            if !name.is_empty() {
                self.profile.set_thread_name(thread_handle, name);
            }
        }

        let thread = ThreadState::new(name, false, thread_handle, thread_label_frame, pid, tid);
        self.threads.insert(tid, thread);
    }

    pub fn handle_thread_start(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        pid: u32,
        name: Option<String>,
    ) {
        if !self.is_interesting_process(pid, None, None) {
            return;
        }

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);

        if let Some(dead_thread_with_reused_tid) = self.threads.remove(&tid) {
            self.profile
                .set_thread_end_time(dead_thread_with_reused_tid.handle, timestamp);
            self.dead_threads_with_reused_tids
                .push(dead_thread_with_reused_tid);
        }

        if !self.processes.contains_key(&pid) {
            log::warn!("Adding thread {tid} for unknown pid {pid}");
            return;
        }

        let process = self.processes.get_mut(&pid).unwrap();
        if !process.seen_main_thread_start {
            process.seen_main_thread_start = true;
            let thread_handle = process.main_thread_handle;
            let thread_label_frame =
                make_thread_label_frame(&mut self.profile, name.as_deref(), pid, tid);
            process.main_thread_label_frame = thread_label_frame.clone();
            // self.profile.set_thread_tid(thread_handle, tid);
            let thread = ThreadState::new(name, true, thread_handle, thread_label_frame, pid, tid);
            self.threads.insert(tid, thread);
            return;
        }

        if self.main_thread_only {
            // Ignore this thread.
            return;
        }

        if let (Some(thread_name), Some(thread_recycler)) =
            (&name, process.thread_recycler.as_mut())
        {
            if let Some((thread_handle, thread_label_frame)) =
                thread_recycler.recycle_by_name(thread_name)
            {
                let thread =
                    ThreadState::new(name, false, thread_handle, thread_label_frame, pid, tid);
                self.threads.insert(tid, thread);
                return;
            }
        }

        let thread_handle = self
            .profile
            .add_thread(process.handle, tid, timestamp, false);
        let thread_label_frame =
            make_thread_label_frame(&mut self.profile, name.as_deref(), pid, tid);
        if let Some(name) = name.as_deref() {
            if !name.is_empty() {
                self.profile.set_thread_name(thread_handle, name);
            }
        }

        let thread = ThreadState::new(name, false, thread_handle, thread_label_frame, pid, tid);
        self.threads.insert(tid, thread);
    }

    pub fn handle_thread_set_name(
        &mut self,
        _timestamp_raw: u64,
        pid: u32,
        tid: u32,
        name: String,
    ) {
        if name.is_empty() {
            return;
        }
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };
        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };

        if let Some(thread_recycler) = process.thread_recycler.as_mut() {
            if let Some(old_name) = thread.name.as_deref() {
                let thread_recycling_data = (thread.handle, thread.label_frame.clone());
                thread_recycler.add_to_pool(old_name, thread_recycling_data);
            }
            if let Some((thread_handle, thread_label_frame)) =
                thread_recycler.recycle_by_name(&name)
            {
                thread.handle = thread_handle;
                thread.label_frame = thread_label_frame;
            }
        }
        self.profile.set_thread_name(thread.handle, &name);
        thread.name = Some(name);
    }

    pub fn handle_thread_end(&mut self, timestamp_raw: u64, pid: u32, tid: u32) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        self.profile.set_thread_end_time(thread.handle, timestamp);

        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };
        if let (Some(name), Some(thread_recycler)) =
            (thread.name.as_deref(), process.thread_recycler.as_mut())
        {
            let thread_recycling_data = (thread.handle, thread.label_frame.clone());
            thread_recycler.add_to_pool(name, thread_recycling_data);
        }
    }

    pub fn handle_thread_dcend(&mut self, _timestamp_raw: u64, _tid: u32) {
        // Nothing to do. The thread is still alive at the end of profiling.
    }

    /// Attach a stack to an existing marker.
    ///
    /// CoreCLR emits these stacks after the corresponding marker.
    pub fn handle_coreclr_stack(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        tid: u32,
        stack_address_iter: impl Iterator<Item = u64>,
        marker_handle: MarkerHandle,
    ) {
        let Some(thread) = self.threads.get(&tid) else {
            return;
        };
        let stack: Vec<StackFrame> = to_stack_frames(stack_address_iter, self.address_classifier);

        let stack_index = self.unresolved_stacks.convert(stack.into_iter().rev());
        //eprintln!("event: StackWalk stack: {:?}", stack);

        // Note: we don't add these as actual samples, and instead just attach them to the marker.
        // If we added them as samples, it would throw off the profile counting, because they arrive
        // in between regular interval samples. In the future, maybe we can support fractional samples
        // somehow (fractional weight), but for now, we just attach them to the marker.

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        self.processes
            .get_mut(&pid)
            .unwrap()
            .unresolved_samples
            .attach_stack_to_marker(
                thread.handle,
                timestamp,
                timestamp_raw,
                stack_index,
                marker_handle,
            );
    }

    pub fn handle_stack_arm64(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        tid: u32,
        stack_address_iter: impl Iterator<Item = u64>,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        // On ARM64, this seems to be simpler -- stacks come in with full kernel and user frames.
        // At least, I've never seen a kernel stack come in separately.
        // TODO -- is this because I can't use PROFILE events in the VM?

        let stack: Vec<StackFrame> = to_stack_frames(stack_address_iter, self.address_classifier);

        let cpu_delta_raw = self
            .context_switch_handler
            .consume_cpu_delta(&mut thread.context_switch_data);
        let cpu_delta =
            CpuDelta::from_nanos(cpu_delta_raw * self.timestamp_converter.raw_to_ns_factor);
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        let stack_index = self.unresolved_stacks.convert(stack.into_iter().rev());
        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };
        process.unresolved_samples.add_sample(
            thread.handle,
            timestamp,
            timestamp_raw,
            stack_index,
            cpu_delta,
            1,
            None,
        );
    }

    pub fn handle_stack_x86(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        tid: u32,
        stack_len: usize,
        stack_address_iter: impl Iterator<Item = u64>,
    ) {
        let mut stack: Vec<StackFrame> = Vec::with_capacity(stack_len);
        let mut address_iter = stack_address_iter;
        let Some(first_frame_address) = address_iter.next() else {
            return;
        };
        let first_frame_stack_mode = self.address_classifier.get_stack_mode(first_frame_address);
        stack.push(StackFrame::InstructionPointer(
            first_frame_address,
            first_frame_stack_mode,
        ));
        stack.extend(address_iter.map(|addr| {
            let stack_mode = self.address_classifier.get_stack_mode(addr);
            StackFrame::ReturnAddress(addr, stack_mode)
        }));

        match first_frame_stack_mode {
            StackMode::Kernel => self.handle_kernel_stack(timestamp_raw, pid, tid, stack),
            StackMode::User => self.handle_user_stack(timestamp_raw, pid, tid, stack),
        }
    }

    fn handle_kernel_stack(
        &mut self,
        timestamp_raw: u64,
        _pid: u32,
        tid: u32,
        stack: Vec<StackFrame>,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };
        if let Some(pending_stack) = thread
            .pending_stacks
            .iter_mut()
            .rev()
            .find(|s| s.timestamp == timestamp_raw)
        {
            if let Some(kernel_stack) = pending_stack.kernel_stack.as_mut() {
                log::warn!("Multiple kernel stacks for timestamp {timestamp_raw} on thread {tid}");
                kernel_stack.extend(&stack);
            } else {
                pending_stack.kernel_stack = Some(stack);
            }
        }
    }

    fn handle_user_stack(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        tid: u32,
        stack: Vec<StackFrame>,
    ) {
        // We now know that we have a user stack. User stacks always come last. Consume
        // the pending stack with matching timestamp.

        let user_stack = stack;
        let user_stack_index = self
            .unresolved_stacks
            .convert(user_stack.iter().cloned().rev());

        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };
        // the number of pending stacks at or before our timestamp
        let num_pending_stacks = thread
            .pending_stacks
            .iter()
            .take_while(|s| s.timestamp <= timestamp_raw)
            .count();

        let pending_stacks: VecDeque<_> =
            thread.pending_stacks.drain(..num_pending_stacks).collect();

        // Use this user stack for all pending stacks from this thread.
        for pending_stack in pending_stacks {
            let PendingStack {
                timestamp: timestamp_raw,
                kernel_stack,
                off_cpu_sample_group,
                on_cpu_sample_cpu_delta,
            } = pending_stack;
            let timestamp = self.timestamp_converter.convert_time(timestamp_raw);

            if let Some(off_cpu_sample_group) = off_cpu_sample_group {
                let OffCpuSampleGroup {
                    begin_timestamp: begin_timestamp_raw,
                    end_timestamp: end_timestamp_raw,
                    sample_count,
                } = off_cpu_sample_group;

                let cpu_delta_raw = {
                    self.context_switch_handler
                        .consume_cpu_delta(&mut thread.context_switch_data)
                };
                let cpu_delta =
                    CpuDelta::from_nanos(cpu_delta_raw * self.timestamp_converter.raw_to_ns_factor);

                // Add a sample at the beginning of the paused range.
                // This "first sample" will carry any leftover accumulated running time ("cpu delta").
                let begin_timestamp = self.timestamp_converter.convert_time(begin_timestamp_raw);
                let Some(process) = self.processes.get_mut(&pid) else {
                    return;
                };
                process.unresolved_samples.add_sample(
                    thread.handle,
                    begin_timestamp,
                    begin_timestamp_raw,
                    user_stack_index,
                    cpu_delta,
                    1,
                    None,
                );

                if sample_count > 1 {
                    // Emit a "rest sample" with a CPU delta of zero covering the rest of the paused range.
                    let weight = i32::try_from(sample_count - 1).unwrap_or(0);
                    let end_timestamp = self.timestamp_converter.convert_time(end_timestamp_raw);
                    process.unresolved_samples.add_sample(
                        thread.handle,
                        end_timestamp,
                        end_timestamp_raw,
                        user_stack_index,
                        CpuDelta::ZERO,
                        weight,
                        None,
                    );
                }
            }

            if let Some(cpu_delta) = on_cpu_sample_cpu_delta {
                if let Some(mut combined_stack) = kernel_stack {
                    combined_stack.extend_from_slice(&user_stack[..]);
                    let combined_stack_index = self
                        .unresolved_stacks
                        .convert(combined_stack.into_iter().rev());
                    let Some(process) = self.processes.get_mut(&pid) else {
                        return;
                    };
                    process.unresolved_samples.add_sample(
                        thread.handle,
                        timestamp,
                        timestamp_raw,
                        combined_stack_index,
                        cpu_delta,
                        1,
                        None,
                    );
                } else {
                    let Some(process) = self.processes.get_mut(&pid) else {
                        return;
                    };
                    process.unresolved_samples.add_sample(
                        thread.handle,
                        timestamp,
                        timestamp_raw,
                        user_stack_index,
                        cpu_delta,
                        1,
                        None,
                    );
                }
                self.stack_sample_count += 1;
            }
        }
    }

    pub fn handle_sample(&mut self, timestamp_raw: u64, tid: u32) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        let off_cpu_sample_group = self
            .context_switch_handler
            .handle_on_cpu_sample(timestamp_raw, &mut thread.context_switch_data);
        let delta = self
            .context_switch_handler
            .consume_cpu_delta(&mut thread.context_switch_data);
        let cpu_delta = CpuDelta::from_nanos(delta * self.timestamp_converter.raw_to_ns_factor);
        thread.pending_stacks.push_back(PendingStack {
            timestamp: timestamp_raw,
            kernel_stack: None,
            off_cpu_sample_group,
            on_cpu_sample_cpu_delta: Some(cpu_delta),
        });

        self.sample_count += 1;
    }

    pub fn handle_virtual_alloc_free(
        &mut self,
        timestamp_raw: u64,
        is_free: bool,
        pid: u32,
        _tid: u32,
        region_size: u64,
        _stringified_properties: String,
    ) {
        if !self.is_interesting_process(pid, None, None) {
            return;
        }

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        let delta_size = if is_free {
            -(region_size as f64)
        } else {
            region_size as f64
        };
        // let op_name = if is_free {
        //     "VirtualFree"
        // } else {
        //     "VirtualAlloc"
        // };

        let Some(memory_usage_counter) = self.get_or_create_memory_usage_counter(pid) else {
            return;
        };
        self.profile
            .add_counter_sample(memory_usage_counter, timestamp, delta_size, 1);
        // TODO: Consider adding a marker here
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_image_load(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        image_base: u64,
        image_size: u32,
        image_timestamp_maybe_zero: u32,
        image_checksum: u32,
        path: String,
        image_info_from_merged_etl: ImageInfoFromMergedEtl,
    ) {
        if !self.is_interesting_process(pid, None, None) && pid != 0 {
            return;
        }

        let path = self.map_device_path(&path);

        let (debug_id, pdb_path, code_id) = if let Some((timestamp, debug_id, pdb_path)) =
            image_info_from_merged_etl.get_complete()
        {
            let code_id = Some(wholesym::CodeId::PeCodeId(PeCodeId {
                timestamp,
                image_size,
            }));
            (debug_id, pdb_path, code_id)
        } else {
            match get_pe_info(Path::new(&path)) {
                Some(pe_info)
                    if pe_info.image_size == image_size
                        && pe_info.image_checksum == image_checksum =>
                {
                    let debug_id = pe_info.debug_id.unwrap_or_default();
                    let pdb_path = pe_info.pdb_path.unwrap_or_else(|| path.clone());
                    let code_id = Some(pe_info.code_id);
                    (debug_id, pdb_path, code_id)
                }
                _ => {
                    // If file doesn't exist or image_size / image_checksum don't match, fall back to default.
                    // This happens for the ghost drivers mentioned here: https://devblogs.microsoft.com/oldnewthing/20160913-00/?p=94305
                    // and for files that were removed since the recording started.
                    let debug_id = DebugId::nil();
                    let pdb_path = path.clone();
                    let code_id = if image_timestamp_maybe_zero != 0 {
                        Some(wholesym::CodeId::PeCodeId(PeCodeId {
                            timestamp: image_timestamp_maybe_zero,
                            image_size,
                        }))
                    } else {
                        None
                    };
                    (debug_id, pdb_path, code_id)
                }
            }
        };

        let name = extract_filename(&path).to_string();
        let debug_name = extract_filename(&pdb_path).to_string();
        let info = LibraryInfo {
            name,
            path,
            debug_name,
            debug_path: pdb_path,
            debug_id,
            code_id: code_id.map(|ci| ci.to_string()),
            arch: Some(self.arch.to_owned()),
            symbol_table: None,
        };
        // attempt to categorize the library based on the path
        let path_lower = info.path.to_lowercase();
        let debug_path_lower = info.debug_path.to_lowercase();

        let known_category = if debug_path_lower.contains(".ni.pdb") {
            KnownCategory::CoreClrR2r
        } else if path_lower.contains("windows\\system32") || path_lower.contains("windows\\winsxs")
        {
            KnownCategory::System
        } else {
            KnownCategory::Unknown
        };

        let lib_handle = self.profile.add_lib(info);
        let start_avma = image_base;
        let end_avma = image_base + image_size as u64;
        if pid == 0 || start_avma >= self.kernel_min {
            self.profile
                .add_kernel_lib_mapping(lib_handle, start_avma, end_avma, 0);
            return;
        }

        let info = if known_category != KnownCategory::Unknown {
            let category = self.categories.get(known_category, &mut self.profile);
            LibMappingInfo::new_lib_with_category(lib_handle, category.into())
        } else {
            LibMappingInfo::new_lib(lib_handle)
        };

        self.processes
            .get_mut(&pid)
            .unwrap()
            .regular_lib_mapping_ops
            .push(
                timestamp_raw,
                LibMappingOp::Add(LibMappingAdd {
                    start_avma,
                    end_avma,
                    relative_address_at_start: 0,
                    info,
                }),
            );
    }

    pub fn handle_vsync(&mut self, timestamp_raw: u64) {
        #[derive(Debug, Clone)]
        pub struct VSyncMarker;

        impl StaticSchemaMarker for VSyncMarker {
            const UNIQUE_MARKER_TYPE_NAME: &'static str = "Vsync";

            fn schema() -> MarkerSchema {
                MarkerSchema {
                    type_name: Self::UNIQUE_MARKER_TYPE_NAME.into(),
                    locations: vec![
                        MarkerLocation::MarkerChart,
                        MarkerLocation::MarkerTable,
                        MarkerLocation::TimelineOverview,
                    ],
                    chart_label: Some("{marker.data.name}".into()),
                    tooltip_label: None,
                    table_label: Some("{marker.name}".into()),
                    fields: vec![],
                    static_fields: vec![],
                }
            }

            fn name(&self, profile: &mut Profile) -> StringHandle {
                profile.intern_string("Vsync")
            }

            fn category(&self, _profile: &mut Profile) -> CategoryHandle {
                CategoryHandle::OTHER
            }

            fn string_field_value(&self, _field_index: u32) -> StringHandle {
                unreachable!()
            }

            fn number_field_value(&self, _field_index: u32) -> f64 {
                unreachable!()
            }
        }

        let gpu_thread = self.gpu_thread_handle.get_or_insert_with(|| {
            let start_timestamp = Timestamp::from_nanos_since_reference(0);
            let gpu = self.profile.add_process("GPU", 1, start_timestamp);
            self.profile.add_thread(gpu, 1, start_timestamp, false)
        });
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        self.profile
            .add_marker(*gpu_thread, MarkerTiming::Instant(timestamp), VSyncMarker);
    }

    pub fn handle_cswitch(&mut self, timestamp_raw: u64, old_tid: u32, new_tid: u32) {
        // println!("CSwitch {} -> {} @ {} on {}", old_tid, old_tid, e.EventHeader.TimeStamp, unsafe { e.BufferContext.Anonymous.ProcessorIndex });

        if let Some(old_thread) = self.threads.get_mut(&old_tid) {
            self.context_switch_handler
                .handle_switch_out(timestamp_raw, &mut old_thread.context_switch_data);
        }
        if let Some(new_thread) = self.threads.get_mut(&new_tid) {
            let off_cpu_sample_group = self
                .context_switch_handler
                .handle_switch_in(timestamp_raw, &mut new_thread.context_switch_data);
            if let Some(off_cpu_sample_group) = off_cpu_sample_group {
                new_thread.pending_stacks.push_back(PendingStack {
                    timestamp: timestamp_raw,
                    kernel_stack: None,
                    off_cpu_sample_group: Some(off_cpu_sample_group),
                    on_cpu_sample_cpu_delta: None,
                });
            }
        }
    }

    pub fn handle_js_method_load(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        method_name: String,
        method_start_address: u64,
        method_size: u32,
    ) {
        if !self.is_interesting_process(pid, None, None) && pid != 0 {
            return;
        }

        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };

        let lib = &mut self.js_jit_lib;
        let (category, js_frame) = self
            .js_category_manager
            .classify_jit_symbol(&method_name, &mut self.profile);
        let info = LibMappingInfo::new_jit_function(lib.lib_handle(), category, js_frame);

        let name_handle = self.profile.intern_string(&method_name);
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        self.profile.add_marker(
            process.main_thread_handle,
            MarkerTiming::Instant(timestamp),
            JitFunctionAddMarker(name_handle),
        );

        process.add_jit_function(
            timestamp_raw,
            lib,
            method_name,
            method_start_address,
            method_size,
            info,
        );
    }

    pub fn handle_coreclr_method_load(
        &mut self,
        timestamp_raw: u64,
        pid: u32,
        method_name: String,
        method_start_address: u64,
        method_size: u32,
    ) {
        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };

        let lib = &mut self.coreclr_jit_lib;
        let info = LibMappingInfo::new_jit_function(lib.lib_handle(), lib.default_category(), None);

        process.add_jit_function(
            timestamp_raw,
            lib,
            method_name,
            method_start_address,
            method_size,
            info,
        );
    }

    pub fn handle_freeform_marker_start(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        name: &str,
        stringified_properties: String,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };
        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        thread.pending_markers.insert(
            name.to_owned(),
            PendingMarker {
                text: stringified_properties,
                start: timestamp,
            },
        );
    }

    pub fn handle_freeform_marker_end(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        name: &str,
        stringified_properties: String,
        known_category: KnownCategory,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);

        // Combine the start and end markers into a single marker.
        // Alternatively, we could output the start marker with IntervalStart, however this has one big drawback:
        // The text stored in the start marker would not be available in the UI!
        // The Firefox Profiler combines IntervalStart and IntervalEnd marker into a single marker
        // whose data is taken only from the *end* marker.
        // So here we manually merge them, taking the data from the *start* marker.
        let (timing, text) = if let Some(pending) = thread.pending_markers.remove(name) {
            (
                MarkerTiming::Interval(pending.start, timestamp),
                pending.text,
            )
        } else {
            (MarkerTiming::IntervalEnd(timestamp), stringified_properties)
        };

        let category = self.categories.get(known_category, &mut self.profile);
        let name = self.profile.intern_string(name.split_once('/').unwrap().1);
        let description = self.profile.intern_string(&text);
        self.profile.add_marker(
            thread.handle,
            timing,
            FreeformMarker(name, description, category),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_firefox_marker(
        &mut self,
        tid: u32,
        marker_name: &str,
        start_time_qpc: u64,
        end_time_qpc: u64,
        phase: Option<u8>,
        maybe_user_timing_name: Option<String>,
        maybe_explicit_marker_name: Option<String>,
        text: String,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        assert!(self.event_timestamps_are_qpc, "Inconsistent timestamp formats! ETW traces with Firefox events should be captured with QPC timestamps (-ClockType PerfCounter) so that ETW sample timestamps are compatible with the QPC timestamps in Firefox ETW trace events, so that the markers appear in the right place.");
        let (phase, instant_time_qpc): (u8, u64) = match phase {
            Some(phase) => (phase, start_time_qpc),
            None => {
                // Before the landing of https://bugzilla.mozilla.org/show_bug.cgi?id=1882640 ,
                // Firefox ETW trace events didn't have phase information, so we need to
                // guess a phase based on the timestamps.
                if start_time_qpc != 0 && end_time_qpc != 0 {
                    (PHASE_INTERVAL, 0)
                } else if start_time_qpc != 0 {
                    (PHASE_INSTANT, start_time_qpc)
                } else {
                    (PHASE_INSTANT, end_time_qpc)
                }
            }
        };
        let timing = match phase {
            PHASE_INSTANT => {
                MarkerTiming::Instant(self.timestamp_converter.convert_time(instant_time_qpc))
            }
            PHASE_INTERVAL => MarkerTiming::Interval(
                self.timestamp_converter.convert_time(start_time_qpc),
                self.timestamp_converter.convert_time(end_time_qpc),
            ),
            PHASE_INTERVAL_START => {
                MarkerTiming::IntervalStart(self.timestamp_converter.convert_time(start_time_qpc))
            }
            PHASE_INTERVAL_END => {
                MarkerTiming::IntervalEnd(self.timestamp_converter.convert_time(end_time_qpc))
            }
            _ => panic!("Unexpected marker phase {phase}"),
        };

        if marker_name == "UserTiming" {
            let name = self.profile.intern_string(&maybe_user_timing_name.unwrap());
            self.profile
                .add_marker(thread.handle, timing, UserTimingMarker(name));
        } else if marker_name == "SimpleMarker" || marker_name == "Text" || marker_name == "tracing"
        {
            let marker_name = self
                .profile
                .intern_string(&maybe_explicit_marker_name.unwrap());
            let description = self.profile.intern_string(&text);
            self.profile.add_marker(
                thread.handle,
                timing,
                FreeformMarker(marker_name, description, CategoryHandle::OTHER),
            );
        } else {
            let marker_name = self.profile.intern_string(marker_name);
            let description = self.profile.intern_string(&text);
            self.profile.add_marker(
                thread.handle,
                timing,
                FreeformMarker(marker_name, description, CategoryHandle::OTHER),
            );
        }
    }

    pub fn handle_chrome_marker(
        &mut self,
        tid: u32,
        marker_name: &str,
        timestamp_raw: u64,
        phase: &str,
        keyword_bitfield: u64,
        text: String,
    ) {
        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        let timestamp = self.timestamp_converter.convert_us(timestamp_raw);

        let timing = match phase {
            "Begin" => MarkerTiming::IntervalStart(timestamp),
            "End" => MarkerTiming::IntervalEnd(timestamp),
            _ => MarkerTiming::Instant(timestamp),
        };
        let keyword = KeywordNames::from_bits(keyword_bitfield).unwrap();
        if keyword == KeywordNames::blink_user_timing {
            let name = self.profile.intern_string(marker_name);
            self.profile
                .add_marker(thread.handle, timing, UserTimingMarker(name));
        } else {
            let marker_name = self.profile.intern_string(marker_name);
            let description = self.profile.intern_string(&text);
            self.profile.add_marker(
                thread.handle,
                timing,
                FreeformMarker(marker_name, description, CategoryHandle::OTHER),
            );
        }
    }
    pub fn handle_unknown_event(
        &mut self,
        timestamp_raw: u64,
        tid: u32,
        task_and_op: &str,
        stringified_properties: String,
    ) {
        if !self.profile_creation_props.unknown_event_markers {
            return;
        }

        let Some(thread) = self.threads.get_mut(&tid) else {
            return;
        };

        let timestamp = self.timestamp_converter.convert_time(timestamp_raw);
        let timing = MarkerTiming::Instant(timestamp);
        // this used to create a new category based on provider_name, just lump them together for now
        let category = self
            .categories
            .get(KnownCategory::Unknown, &mut self.profile);
        let marker_name = self.profile.intern_string(task_and_op);
        let description = self.profile.intern_string(&stringified_properties);
        self.profile.add_marker(
            thread.handle,
            timing,
            FreeformMarker(marker_name, description, category),
        );
        //println!("unhandled {}", s.name())
    }

    pub fn is_in_time_range(&self, ts_raw: u64) -> bool {
        let Some((tstart, tstop)) = self.time_range else {
            return true;
        };

        let ts = self.timestamp_converter.convert_time(ts_raw);
        ts >= tstart && ts < tstop
    }

    pub fn set_os_name(&mut self, os_name: &str) {
        self.profile.set_os_name(os_name);
    }

    pub fn finish(mut self) -> Profile {
        // Push queued samples into the profile.
        // We queue them so that we can get symbolicated JIT function names. To get symbolicated JIT function names,
        // we have to call profile.add_sample after we call profile.set_lib_symbol_table, and we don't have the
        // complete JIT symbol table before we've seen all JIT symbols.
        // (This is a rather weak justification. The better justification is that this is consistent with what
        // samply does on Linux and macOS, where the queued samples also want to respect JIT function names from
        // a /tmp/perf-1234.map file, and this file may not exist until the profiled process finishes.)
        let mut stack_frame_scratch_buf = Vec::new();
        self.js_jit_lib
            .finish_and_set_symbol_table(&mut self.profile);
        self.coreclr_jit_lib
            .finish_and_set_symbol_table(&mut self.profile);
        let process_iter = self
            .dead_processes_with_reused_pids
            .into_iter()
            .chain(self.processes.into_values());
        let process_sample_datas: Vec<_> = process_iter
            .map(|process| {
                let jitdump_lib_mapping_op_queues = if !process.jit_lib_mapping_ops.is_empty() {
                    vec![process.jit_lib_mapping_ops]
                } else {
                    Vec::new()
                };

                ProcessSampleData::new(
                    process.unresolved_samples,
                    process.regular_lib_mapping_ops,
                    jitdump_lib_mapping_op_queues,
                    None,
                    Vec::new(),
                )
            })
            .collect();

        let user_category = self.categories.get(KnownCategory::User, &mut self.profile);
        let kernel_category = self
            .categories
            .get(KnownCategory::Kernel, &mut self.profile);

        for process_sample_data in process_sample_datas {
            process_sample_data.flush_samples_to_profile(
                &mut self.profile,
                user_category.into(),
                kernel_category.into(),
                &mut stack_frame_scratch_buf,
                &self.unresolved_stacks,
            )
        }

        log::info!(
            "{} events, {} samples, {} stack-samples",
            self.event_count,
            self.sample_count,
            self.stack_sample_count
        );

        self.profile
    }
}

struct PeInfo {
    code_id: wholesym::CodeId,
    image_size: u32,
    image_checksum: u32,
    debug_id: Option<DebugId>,
    pdb_path: Option<String>,
}

fn get_pe_info(image_path: &Path) -> Option<PeInfo> {
    let file = std::fs::File::open(image_path).ok()?;
    let mmap = unsafe { memmap2::Mmap::map(&file).ok()? };
    let info = match object::read::FileKind::parse(&mmap[..]).ok()? {
        object::FileKind::Pe32 => pe_info(&object::read::pe::PeFile32::parse(&mmap[..]).ok()?),
        object::FileKind::Pe64 => pe_info(&object::read::pe::PeFile64::parse(&mmap[..]).ok()?),
        kind => {
            log::warn!("Unexpected file kind {kind:?} for image file at {image_path:?}");
            return None;
        }
    };
    Some(info)
}

fn pe_info<'a, Pe: object::read::pe::ImageNtHeaders, R: object::ReadRef<'a>>(
    pe: &object::read::pe::PeFile<'a, Pe, R>,
) -> PeInfo {
    // The code identifier consists of the `time_date_stamp` field id the COFF header, followed by
    // the `size_of_image` field in the optional header. If the optional PE header is not present,
    // this identifier is `None`.
    let header = pe.nt_headers();
    let timestamp = header
        .file_header()
        .time_date_stamp
        .get(object::LittleEndian);
    use object::read::pe::ImageOptionalHeader;
    let image_size = header.optional_header().size_of_image();
    let image_checksum = header.optional_header().check_sum();
    let code_id = wholesym::CodeId::PeCodeId(wholesym::PeCodeId {
        timestamp,
        image_size,
    });

    use object::Object;
    let pdb_info = pe.pdb_info().ok().flatten();
    let pdb_path: Option<String> = pdb_info.and_then(|pdb_info| {
        let pdb_path = std::str::from_utf8(pdb_info.path()).ok()?;
        Some(pdb_path.to_string())
    });
    let debug_id: Option<DebugId> =
        pdb_info.and_then(|pdb_info| DebugId::from_guid_age(&pdb_info.guid(), pdb_info.age()).ok());

    PeInfo {
        code_id,
        image_size,
        image_checksum,
        debug_id,
        pdb_path,
    }
}

fn to_stack_frames(
    mut address_iter: impl Iterator<Item = u64>,
    address_classifier: AddressClassifier,
) -> Vec<StackFrame> {
    let Some(first_addr) = address_iter.next() else {
        return Vec::new();
    };
    let first_stack_mode = address_classifier.get_stack_mode(first_addr);
    let mut frames = vec![StackFrame::InstructionPointer(first_addr, first_stack_mode)];

    frames.extend(address_iter.map(|addr| {
        let stack_mode = address_classifier.get_stack_mode(addr);
        StackFrame::ReturnAddress(addr, stack_mode)
    }));
    frames
}

pub fn make_thread_label_frame(
    profile: &mut Profile,
    name: Option<&str>,
    pid: u32,
    tid: u32,
) -> FrameInfo {
    let s = match name {
        Some(name) => format!("{name} (pid: {pid}, tid: {tid})"),
        None => format!("Thread {tid} (pid: {pid}, tid: {tid})"),
    };
    let thread_label = profile.intern_string(&s);
    FrameInfo {
        frame: Frame::Label(thread_label),
        category_pair: CategoryHandle::OTHER.into(),
        flags: FrameFlags::empty(),
    }
}

#[derive(Debug, Clone)]
pub struct FreeformMarker(StringHandle, StringHandle, CategoryHandle);

impl StaticSchemaMarker for FreeformMarker {
    const UNIQUE_MARKER_TYPE_NAME: &'static str = "FreeformMarker";

    fn schema() -> MarkerSchema {
        MarkerSchema {
            type_name: Self::UNIQUE_MARKER_TYPE_NAME.into(),
            locations: vec![MarkerLocation::MarkerChart, MarkerLocation::MarkerTable],
            chart_label: Some("{marker.data.values}".into()),
            tooltip_label: Some("{marker.name} - {marker.data.values}".into()),
            table_label: Some("{marker.data.values}".into()),
            fields: vec![MarkerFieldSchema {
                key: "values".into(),
                label: "Values".into(),
                format: MarkerFieldFormat::String,
                searchable: true,
            }],
            static_fields: vec![],
        }
    }

    fn name(&self, _profile: &mut Profile) -> StringHandle {
        self.0
    }

    fn category(&self, _profile: &mut Profile) -> CategoryHandle {
        self.2
    }

    fn string_field_value(&self, _field_index: u32) -> StringHandle {
        self.1
    }

    fn number_field_value(&self, _field_index: u32) -> f64 {
        unreachable!()
    }
}

fn extract_filename(path: &str) -> &str {
    match path.rsplit_once(['/', '\\']) {
        Some((_base, file_name)) => file_name,
        None => path,
    }
}
