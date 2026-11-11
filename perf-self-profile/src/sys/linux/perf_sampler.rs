//! perf_event_open-based CPU sampler backend.
//!
//! Uses the kernel's `perf_event_open` syscall with mmap ring buffers for
//! zero-copy sample delivery. Each event fd has its own ring buffer; samples
//! are parsed via `perf-event-data`.

use std::io;
use std::ptr;

use perf_event_data::Record;
use perf_event_data::endian::Little;
use perf_event_data::parse::{ParseConfig, Parser};
use perf_event_open_sys::bindings::{
    PERF_CONTEXT_MAX as PERF_CONTEXT_START_MARKER, PERF_CONTEXT_USER, PERF_COUNT_HW_CPU_CYCLES,
    PERF_COUNT_SW_CONTEXT_SWITCHES, PERF_COUNT_SW_CPU_CLOCK, PERF_COUNT_SW_TASK_CLOCK,
    PERF_FLAG_FD_CLOEXEC, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CPU, PERF_SAMPLE_IP,
    PERF_SAMPLE_PERIOD, PERF_SAMPLE_RAW, PERF_SAMPLE_REGS_USER, PERF_SAMPLE_TID, PERF_SAMPLE_TIME,
    PERF_TYPE_HARDWARE, PERF_TYPE_SOFTWARE, PERF_TYPE_TRACEPOINT, perf_event_attr,
};

use super::USER_ADDR_LIMIT;
use super::call_site_addr;
use super::gettid;
use super::ring_buffer::{RingBuffer, page_size};
use crate::sampler::{EventSource, Sample, SamplerConfig};

struct PerfEvent {
    fd: i32,
    ring: RingBuffer,
    /// Thread ID this event is tracking, or 0 for process-wide events.
    tid: i32,
}

impl Drop for PerfEvent {
    fn drop(&mut self) {
        unsafe { perf_event_open_sys::ioctls::DISABLE(self.fd, 0) };
        // RingBuffer::drop handles munmap; closing the fd after munmap is fine on Linux.
        unsafe { libc::close(self.fd) };
    }
}

const PAGE_COUNT: usize = 512; // power of 2; 2 MB at 4 KB pages

/// Open a perf event fd, mmap the ring buffer, and enable it.
fn open_perf_event(attr: &mut perf_event_attr, pid: i32, cpu: i32) -> io::Result<PerfEvent> {
    let page_size = page_size();
    let data_size = PAGE_COUNT * page_size;
    let mmap_size = (PAGE_COUNT + 1) * page_size;

    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            attr,
            pid,
            cpu,
            -1,
            PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            mmap_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    if base == libc::MAP_FAILED {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let ring = unsafe { RingBuffer::new(base as *mut u8, data_size as u64, mmap_size) };

    if unsafe { perf_event_open_sys::ioctls::ENABLE(fd, 0) } < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    Ok(PerfEvent { fd, ring, tid: pid })
}

pub(super) struct PerfSamplerImpl {
    events: Vec<PerfEvent>,
    parse_config: ParseConfig<Little>,
    attr: perf_event_attr,
    include_kernel: bool,
    max_tracked_threads: usize,
}

impl PerfSamplerImpl {
    pub(super) fn start_for_pid(pid: i32, config: &SamplerConfig) -> io::Result<Self> {
        let mut attr = Self::build_attr(config)?;

        let is_event_based = matches!(
            config.event_source,
            EventSource::SwContextSwitches | EventSource::Tracepoint(_)
        );
        let mut events = Vec::new();

        if is_event_based {
            // Single fd, cpu=-1, pid=target process
            events.push(open_perf_event(&mut attr, pid, -1)?);
        } else {
            // With inherit + sampling, the kernel forbids cpu=-1 for mmap. We open
            // one event per CPU in the process's affinity mask. Using affinity
            // rather than all online CPUs avoids opening fds for CPUs the process
            // can never run on, reducing inherited-context overhead on thread
            // create/exit (see https://github.com/dial9-rs/dial9/issues/579).
            let cpus = get_affinity_cpus()?;
            events.reserve(cpus.len());
            for &cpu in &cpus {
                events.push(open_perf_event(&mut attr, pid, cpu)?);
            }
        }

        Ok(PerfSamplerImpl {
            events,
            parse_config: ParseConfig::from(attr),
            attr,
            include_kernel: config.include_kernel,
            max_tracked_threads: config.max_tracked_threads,
        })
    }

    pub(super) fn new_per_thread(config: &SamplerConfig) -> io::Result<Self> {
        let mut attr = Self::build_attr(config)?;

        // Probe perf_event_open to detect if it's blocked (e.g., seccomp on
        // ECS/Fargate). Without this, creation succeeds but every subsequent
        // track_current_thread() call fails silently.
        let probe_fd = unsafe {
            perf_event_open_sys::perf_event_open(
                &mut attr,
                0,  // current thread
                -1, // any CPU
                -1, // no group
                PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
            )
        };
        if probe_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::close(probe_fd) };

        Ok(PerfSamplerImpl {
            events: Vec::new(),
            parse_config: ParseConfig::from(attr),
            attr,
            include_kernel: config.include_kernel,
            max_tracked_threads: config.max_tracked_threads,
        })
    }

    fn build_attr(config: &SamplerConfig) -> io::Result<perf_event_attr> {
        use crate::SamplingMode;

        match config.sampling {
            SamplingMode::FrequencyHz(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "FrequencyHz(0) is invalid",
                ));
            }
            SamplingMode::FrequencyHz(hz) => {
                // Check max sample rate
                if let Ok(contents) =
                    std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
                    && let Ok(max_rate) = contents.trim().parse::<u64>()
                    && hz > max_rate
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "requested frequency {} exceeds kernel max {} \
                             (see /proc/sys/kernel/perf_event_max_sample_rate)",
                            hz, max_rate
                        ),
                    ));
                }
            }
            SamplingMode::Period(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Period must be >= 1",
                ));
            }
            SamplingMode::Period(_) => {}
        }

        let mut attr = perf_event_attr::default();
        attr.size = std::mem::size_of::<perf_event_attr>() as u32;

        match config.event_source {
            EventSource::HwCpuCycles => {
                attr.type_ = PERF_TYPE_HARDWARE;
                attr.config = PERF_COUNT_HW_CPU_CYCLES as u64;
            }
            EventSource::SwCpuClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CPU_CLOCK as u64;
            }
            EventSource::SwTaskClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_TASK_CLOCK as u64;
            }
            EventSource::SwContextSwitches => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CONTEXT_SWITCHES as u64;
            }
            EventSource::Tracepoint(id) => {
                attr.type_ = PERF_TYPE_TRACEPOINT;
                attr.config = id as u64;
            }
        }

        let is_event_based = matches!(
            config.event_source,
            EventSource::SwContextSwitches | EventSource::Tracepoint(_)
        );

        attr.sample_type = PERF_SAMPLE_IP as u64
            | PERF_SAMPLE_CALLCHAIN as u64
            | PERF_SAMPLE_TID as u64
            | PERF_SAMPLE_TIME as u64
            | PERF_SAMPLE_CPU as u64
            // Omit PERF_SAMPLE_PERIOD in Period mode. When set, perf_swevent_event()
            // emits a sample on every SW event, skipping the countdown
            // that implements 1-in-N sampling.
            | if matches!(config.sampling, SamplingMode::FrequencyHz(_)) {
                PERF_SAMPLE_PERIOD as u64
            } else {
                0
            }
            // PERF_SAMPLE_RAW includes the tracepoint's raw event data (field
            // values) in each sample. Only tracepoints produce meaningful raw
            // data; CPU and context-switch sources have nothing to attach.
            | if matches!(config.event_source, EventSource::Tracepoint(_)) {
                PERF_SAMPLE_RAW as u64
            } else {
                0
            }
            // aarch64: the kernel's callchain walks the x29 chain only, so a
            // frameless leaf loses its immediate caller. Record x30 (LR), one
            // extra u64 per sample, and re-insert the frame in
            // `insert_leaf_caller`.
            | if cfg!(target_arch = "aarch64") {
                PERF_SAMPLE_REGS_USER as u64
            } else {
                0
            };

        #[cfg(target_arch = "aarch64")]
        {
            attr.sample_regs_user = 1u64 << PERF_REG_ARM64_LR;
        }

        // Stamp samples with CLOCK_MONOTONIC so they share a clock with ctimer
        // samples and `telemetry::events::clock_monotonic_ns()`.
        attr.set_use_clockid(1);
        attr.clockid = libc::CLOCK_MONOTONIC;

        attr.set_disabled(1);
        match config.sampling {
            SamplingMode::Period(period) => {
                // exclude_kernel must remain 0 since context switches fire in
                // kernel context; kernel callchain frames are filtered at
                // parse time via USER_ADDR_LIMIT.
                attr.sample_period = period;
                attr.wakeup_events = 1;
                attr.set_sample_id_all(1);
            }
            SamplingMode::FrequencyHz(hz) => {
                attr.sample_freq = hz;
                attr.set_freq(1);
                attr.set_sample_id_all(1);
                if !is_event_based {
                    attr.set_inherit(1);
                }
                if excludes_kernel(config) {
                    attr.set_exclude_kernel(1);
                    attr.set_exclude_hv(1);
                }
            }
        }

        Ok(attr)
    }
}

/// Whether events opened for `config` exclude the kernel, which lets them open
/// under `perf_event_paranoid` 2.
pub(super) fn excludes_kernel(config: &SamplerConfig) -> bool {
    matches!(config.sampling, crate::SamplingMode::FrequencyHz(_))
        && !matches!(
            config.event_source,
            EventSource::SwContextSwitches | EventSource::Tracepoint(_)
        )
        && !config.include_kernel
}

impl super::sampler::SamplerBackend for PerfSamplerImpl {
    fn active_backend(&self) -> crate::sampler::ActiveCpuBackend {
        crate::sampler::ActiveCpuBackend::Perf
    }

    // Per-thread mode: call from the thread you want to monitor.
    // This opens an event fd scoped to the calling tid with cpu=-1.
    fn track_current_thread(&mut self) -> io::Result<()> {
        if self.events.len() >= self.max_tracked_threads {
            return Err(io::Error::other(format!(
                "perf sampler: max tracked threads ({}) reached, \
                 refusing to open another fd",
                self.max_tracked_threads
            )));
        }
        let mut ev = open_perf_event(&mut self.attr, 0, -1)?;
        // open_perf_event stores tid=0 (the "current thread" sentinel passed to
        // perf_event_open). Resolve to the real tid so stop_tracking_current_thread
        // can find this event later.
        ev.tid = gettid() as i32;
        self.events.push(ev);
        Ok(())
    }

    /// Must be called from the same thread that called track_current_thread.
    /// Dropping the event closes fd and unmaps ring, unread samples are lost.
    fn stop_tracking_current_thread(&mut self) {
        let tid = gettid() as i32;
        if let Some(idx) = self.events.iter().position(|ev| ev.tid == tid) {
            self.events.swap_remove(idx);
        }
    }

    fn has_pending(&self) -> bool {
        self.events.iter().any(|ev| ev.ring.has_data())
    }

    /// Non-blocking drain of all currently available records.
    fn for_each_sample(&mut self, f: &mut dyn FnMut(&Sample)) {
        let parse_config = self.parse_config.clone();

        for ev in &mut self.events {
            ev.ring.for_each_record(|record| {
                let mut buf = Vec::with_capacity(record.header.size as usize);
                let header_bytes: [u8; 8] = unsafe { std::mem::transmute(record.header) };
                buf.extend_from_slice(&header_bytes);
                match &record.body {
                    super::ring_buffer::RecordBody::Contiguous(data) => buf.extend_from_slice(data),
                    super::ring_buffer::RecordBody::Split(a, b) => {
                        buf.extend_from_slice(a);
                        buf.extend_from_slice(b);
                    }
                }

                let mut parser = Parser::new(buf.as_slice(), parse_config.clone());
                let parsed = match parser.parse::<Record>() {
                    Ok(r) => r,
                    Err(_) => return,
                };

                let sample = match parsed {
                    Record::Sample(s) => {
                        let include_kernel = self.include_kernel;
                        let (mut callchain, user_pc) =
                            filter_callchain(s.callchain().unwrap_or(&[]), include_kernel);
                        recover_leaf_caller(&mut callchain, user_pc, &s);
                        Sample {
                            ip: s.ip().unwrap_or(0),
                            pid: s.pid().unwrap_or(0),
                            tid: s.tid().unwrap_or(0),
                            time: s.time().unwrap_or(0),
                            cpu: s.cpu(),
                            period: s.period().unwrap_or(0),
                            callchain,
                            raw: s.raw().map(|r| r.to_vec()),
                        }
                    }
                    Record::Lost(lost) => {
                        tracing::debug!("[perf] lost {} events (ring buffer overflow)", lost.lost);
                        return;
                    }
                    _ => return,
                };

                f(&sample);
            });
        }
    }

    fn drain_samples(&mut self) -> Vec<Sample> {
        let mut samples = Vec::new();
        self.for_each_sample(&mut |s| samples.push(s.clone()));
        samples
    }

    /// Pause collection. Ring buffers remain mapped and pending samples can
    /// still be drained. Reversible via `enable`.
    fn disable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::DISABLE(ev.fd, 0) };
        }
    }

    /// Resume collection after disable().
    fn enable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::ENABLE(ev.fd, 0) };
        }
    }
}

/// `PERF_REG_ARM64_LR` from `arch/arm64/include/uapi/asm/perf_regs.h`: x30.
#[cfg(target_arch = "aarch64")]
const PERF_REG_ARM64_LR: u32 = 30;

/// Link register from a sample's recorded user registers. `Registers::regs`
/// holds only the requested registers in ascending mask-bit order, so the
/// index is the count of set mask bits below LR.
///
/// PAC is stripped here: the kernel strips it from the addresses it puts in the
/// callchain but not from `PERF_SAMPLE_REGS_USER`. Stripping is what lets a
/// signed caller be recovered at all — left signed it resolves outside the text
/// and the frame is dropped.
#[cfg(target_arch = "aarch64")]
fn lr_from_regs(regs: &perf_event_data::Registers<'_>) -> Option<u64> {
    let bit = 1u64 << PERF_REG_ARM64_LR;
    if regs.mask & bit == 0 {
        return None;
    }
    let idx = (regs.mask & (bit - 1)).count_ones() as usize;
    let lr = *regs.regs.get(idx)?;
    Some(crate::sys::fp_profiler::unwind::strip_pac(lr as usize) as u64)
}

/// Repair the chain's dropped leaf-caller frame, where the platform can.
///
/// Only the aarch64 walks lose that frame, so this is a no-op elsewhere; the
/// `cfg` lives here rather than at the call site so the drain reads straight
/// through.
#[cfg(target_arch = "aarch64")]
fn recover_leaf_caller(chain: &mut Vec<u64>, user_pc: usize, s: &perf_event_data::Sample<'_>) {
    if let Some(lr) = s.regs_user().and_then(lr_from_regs) {
        insert_leaf_caller(chain, user_pc, lr, s.pid());
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn recover_leaf_caller(_chain: &mut Vec<u64>, _user_pc: usize, _s: &perf_event_data::Sample<'_>) {}

/// Re-insert the immediate caller the kernel's x29-chain walk dropped.
///
/// `chain` is already filtered by [`filter_callchain`], so its entries are call
/// sites and `lr` is converted to one before comparing. The recovered frame
/// belongs next to the sampled *user* PC: with `include_kernel` the chain
/// starts with kernel frames, and `user_pc` is the index of the first user
/// entry rather than 0. The decision is
/// [`crate::sys::func_starts::lr_is_missing_caller`], shared with the ctimer
/// backend's walk.
///
/// The table describes `/proc/self/maps`, so it applies only to this process's
/// samples. `start_for_pid` and `inherit` can deliver samples from others,
/// whose addresses would be judged against our symbol boundaries and could
/// resolve to one of our functions. A foreign sample gets no table and takes
/// the same path as a missing one: the chain is left alone.
///
/// Drain is caller context, so the table is built on demand — a perf-only
/// process never pays for it at startup, and a foreign sample never builds it.
#[cfg(target_arch = "aarch64")]
fn insert_leaf_caller(chain: &mut Vec<u64>, user_pc: usize, lr: u64, sample_pid: Option<u32>) {
    let table =
        (sample_pid == Some(std::process::id())).then(crate::sys::func_starts::get_or_build);
    insert_leaf_caller_with(chain, user_pc, lr, table);
}

/// [`insert_leaf_caller`] with the table supplied, so it can be tested without
/// one built from this process.
#[cfg(target_arch = "aarch64")]
fn insert_leaf_caller_with(
    chain: &mut Vec<u64>,
    user_pc: usize,
    lr: u64,
    table: Option<&crate::sys::func_starts::FuncStarts>,
) {
    let Some(&pc) = chain.get(user_pc) else {
        return;
    };
    if lr == 0 {
        return;
    }
    let site = call_site_addr(lr);
    let chain_site = chain.get(user_pc + 1).copied();
    if crate::sys::func_starts::lr_is_missing_caller(pc, site, chain_site, table) {
        chain.insert(user_pc + 1, site);
    }
}

/// Filter a raw perf callchain, removing zero addresses, perf context markers
/// (PERF_CONTEXT_KERNEL, PERF_CONTEXT_USER, etc.), and optionally kernel addresses.
/// User return addresses become call-site addresses.
/// Returns the filtered chain and the index of the sampled user PC within it,
/// which is 0 unless kernel frames were kept ahead of it.
fn filter_callchain(raw: &[u64], include_kernel: bool) -> (Vec<u64>, usize) {
    let sampled_pc = raw
        .iter()
        .position(|&a| a == PERF_CONTEXT_USER)
        .map_or(0, |marker| marker + 1);
    let mut out = Vec::with_capacity(raw.len());
    let mut user_pc = 0;
    for (i, &a) in raw.iter().enumerate() {
        let addr = if i == sampled_pc || a >= USER_ADDR_LIMIT {
            a
        } else {
            call_site_addr(a)
        };
        if addr == 0
            || addr >= PERF_CONTEXT_START_MARKER
            || (!include_kernel && addr >= USER_ADDR_LIMIT)
        {
            continue;
        }
        if i == sampled_pc {
            user_pc = out.len();
        }
        out.push(addr);
    }
    (out, user_pc)
}

/// Get the CPUs in this process's effective affinity mask from /proc/self/status Cpus_allowed_list.
/// Format is like "0-7" or "0-3,5,7-11".
fn get_affinity_cpus() -> io::Result<Vec<i32>> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    for line in status.lines() {
        if let Some(list) = line.strip_prefix("Cpus_allowed_list:\t") {
            return parse_cpu_list(list.trim());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Cpus_allowed_list not found in /proc/self/status",
    ))
}

/// Parse a CPU list string like "0-7" or "0-3,5,7-11" into a sorted Vec of CPU indices.
fn parse_cpu_list(s: &str) -> io::Result<Vec<i32>> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let start: i32 = start.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu range: {e}"))
            })?;
            let end: i32 = end.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu range: {e}"))
            })?;
            cpus.extend(start..=end);
        } else {
            let cpu: i32 = part.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu id: {e}"))
            })?;
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

#[cfg(all(test, target_arch = "aarch64"))]
mod lr_regs_tests {
    use super::*;
    use std::borrow::Cow;

    fn regs(mask: u64, values: &[u64]) -> perf_event_data::Registers<'static> {
        perf_event_data::Registers {
            abi: perf_event_data::SampleRegsAbi::ABI_64,
            mask,
            regs: Cow::Owned(values.to_vec()),
        }
    }

    /// The kernel does not strip PAC from `PERF_SAMPLE_REGS_USER`, so a signed
    /// LR arrives here with bits set above the address. Left in place it passes
    /// every bound check and names a bogus function.
    #[test]
    fn strips_pac_from_lr() {
        let signed = 0x00aa_0000_0040_2020u64;
        let lr = lr_from_regs(&regs(1 << PERF_REG_ARM64_LR, &[signed]));
        assert_eq!(lr, Some(0x0000_0000_0040_2020));
    }

    /// Only requested registers are present, so the index is the number of set
    /// mask bits below LR, not the register number.
    #[test]
    fn indexes_lr_by_mask_position() {
        let mask = (1 << 0) | (1 << 29) | (1 << PERF_REG_ARM64_LR);
        let lr = lr_from_regs(&regs(mask, &[0x11, 0x22, 0x40_2020]));
        assert_eq!(lr, Some(0x40_2020));
    }

    #[test]
    fn absent_lr_is_none() {
        assert_eq!(lr_from_regs(&regs(1 << 29, &[0x22])), None);
        // Mask claims LR but the payload is short.
        assert_eq!(lr_from_regs(&regs(1 << PERF_REG_ARM64_LR, &[])), None);
    }
}

/// `filter_callchain`'s user-PC index, and `insert_leaf_caller` using it. The
/// index is 0 only when kernel frames were filtered out; with `include_kernel`
/// the rule has to be applied further in, or every such sample silently keeps
/// the dropped frame.
#[cfg(all(test, target_arch = "aarch64"))]
mod user_pc_tests {
    use super::*;
    use crate::sys::func_starts::FuncStarts;

    const LEAF: u64 = 0x40_1000;
    const CALLER: u64 = 0x40_2000;
    const GRANDPARENT: u64 = 0x40_3000;
    const TEXT_END: u64 = 0x40_4000;

    fn table() -> FuncStarts {
        FuncStarts::from_sorted_for_test(vec![LEAF, CALLER, GRANDPARENT], TEXT_END)
    }

    #[test]
    fn user_pc_is_zero_without_kernel_frames() {
        let raw = [PERF_CONTEXT_USER, LEAF + 0x10, GRANDPARENT + 0x30];
        let (chain, user_pc) = filter_callchain(&raw, false);
        assert_eq!(user_pc, 0);
        assert_eq!(chain, vec![LEAF + 0x10, GRANDPARENT + 0x30 - 1]);
    }

    #[test]
    fn user_pc_skips_kept_kernel_frames() {
        let k = USER_ADDR_LIMIT + 0x100;
        // Any value at or above the marker base is a context marker and is
        // filtered out; only PERF_CONTEXT_USER is needed to locate the user PC.
        let raw = [
            PERF_CONTEXT_START_MARKER,
            k,
            k + 4,
            PERF_CONTEXT_USER,
            LEAF + 0x10,
            GRANDPARENT + 0x30,
        ];
        let (chain, user_pc) = filter_callchain(&raw, true);
        assert_eq!(user_pc, 2);
        assert_eq!(chain[user_pc], LEAF + 0x10);
    }

    #[test]
    fn inserts_at_the_user_pc_not_index_zero() {
        let k = USER_ADDR_LIMIT + 0x100;
        let mut chain = vec![k, k + 4, LEAF + 0x10, GRANDPARENT + 0x30];
        insert_leaf_caller_with(&mut chain, 2, CALLER + 0x20, Some(&table()));
        assert_eq!(
            chain,
            vec![k, k + 4, LEAF + 0x10, CALLER + 0x20 - 1, GRANDPARENT + 0x30]
        );
    }

    #[inline(never)]
    extern "C" fn gate_callee(x: u64) -> u64 {
        std::hint::black_box(x).wrapping_add(1)
    }

    #[inline(never)]
    extern "C" fn gate_caller(x: u64) -> u64 {
        gate_callee(x).wrapping_add(2)
    }

    /// A sample from another process must be left alone, and one from this
    /// process must be repaired.
    ///
    /// Real function addresses, not synthetic ones: the table is built from
    /// this process's own `/proc/self/maps`, so made-up addresses resolve to
    /// nothing and the gate's two branches would behave identically.
    #[test]
    fn the_pid_gate_decides_whether_a_sample_is_repaired() {
        let pc = gate_callee as *const () as u64 + 4;
        let lr = gate_caller as *const () as u64 + 8;
        let outer = gate_caller as *const () as u64 + 16;
        let chain = vec![pc, outer];

        // Our own pid: the caller derived from LR is inserted.
        let mut ours = chain.clone();
        insert_leaf_caller(&mut ours, 0, lr, Some(std::process::id()));
        assert_eq!(
            ours,
            vec![pc, call_site_addr(lr), outer],
            "a sample from this process must have its caller restored"
        );

        // Another pid, or none: the table says nothing about those addresses.
        for pid in [Some(std::process::id().wrapping_add(1)), None] {
            let mut other = chain.clone();
            insert_leaf_caller(&mut other, 0, lr, pid);
            assert_eq!(other, chain, "pid {pid:?} must be left alone");
        }
    }

    /// The chain already reports the caller, so inserting would duplicate it.
    /// This is the comparison that needs `lr` in call-site form.
    #[test]
    fn skips_when_the_chain_already_has_the_call_site() {
        let site = call_site_addr(CALLER + 0x20);
        let mut chain = vec![LEAF + 0x10, site];
        let before = chain.clone();
        insert_leaf_caller_with(&mut chain, 0, CALLER + 0x20, Some(&table()));
        assert_eq!(chain, before);
    }

    #[test]
    fn handles_a_chain_that_ends_at_the_pc() {
        let mut chain = vec![LEAF + 0x10];
        insert_leaf_caller_with(&mut chain, 0, CALLER + 0x20, Some(&table()));
        assert_eq!(chain, vec![LEAF + 0x10, CALLER + 0x20 - 1]);
    }

    #[test]
    fn does_nothing_for_an_out_of_range_user_pc() {
        let mut chain = vec![LEAF + 0x10];
        insert_leaf_caller_with(&mut chain, 5, CALLER + 0x20, Some(&table()));
        assert_eq!(chain, vec![LEAF + 0x10]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_callchain_removes_zeros_and_context_markers() {
        let raw = [PERF_CONTEXT_USER, 0x1000, 0, 0x2001, 0];
        let (result, _) = filter_callchain(&raw, false);
        assert_eq!(result, vec![0x1000, 0x2000]);
    }

    #[test]
    fn filter_callchain_excludes_kernel_addrs_when_not_included() {
        let kernel_addr = USER_ADDR_LIMIT + 0x1000;
        let user_addr = 0x5555_0000_1000u64;
        let raw = [user_addr, kernel_addr];
        assert_eq!(filter_callchain(&raw, false).0, vec![user_addr]);
    }

    #[test]
    fn filter_callchain_includes_kernel_addrs_when_included() {
        let kernel_addr = USER_ADDR_LIMIT + 0x1000;
        let user_addr = 0x5555_0000_1000u64;
        let raw = [user_addr, kernel_addr];
        assert_eq!(filter_callchain(&raw, true).0, vec![user_addr, kernel_addr]);
    }

    #[test]
    fn filter_callchain_adjusts_user_return_addrs_but_not_the_sampled_pc() {
        let (pc, ret) = (0x5555_0000_1000u64, 0x5555_0000_2000u64);
        let raw = [PERF_CONTEXT_USER, pc, ret];
        assert_eq!(filter_callchain(&raw, false).0, vec![pc, ret - 1]);
    }

    #[test]
    fn filter_callchain_rejects_all_context_markers() {
        let markers = [
            PERF_CONTEXT_START_MARKER,
            PERF_CONTEXT_START_MARKER + 1,
            u64::MAX,
            u64::MAX - 128,
        ];
        assert!(filter_callchain(&markers, true).0.is_empty());
    }

    #[test]
    fn parse_cpu_list_single() {
        assert_eq!(parse_cpu_list("0").unwrap(), vec![0]);
        assert_eq!(parse_cpu_list("5").unwrap(), vec![5]);
    }

    #[test]
    fn parse_cpu_list_range() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn parse_cpu_list_mixed() {
        assert_eq!(
            parse_cpu_list("0-3,5,7-9").unwrap(),
            vec![0, 1, 2, 3, 5, 7, 8, 9]
        );
    }

    #[test]
    fn get_affinity_cpus_returns_valid_set() {
        let affinity = get_affinity_cpus().expect("should read affinity");
        assert!(
            !affinity.is_empty(),
            "affinity should have at least one CPU"
        );
        assert!(affinity.iter().all(|&c| c >= 0));
        for w in affinity.windows(2) {
            assert!(
                w[0] < w[1],
                "expected sorted unique CPUs, got {:?}",
                affinity
            );
        }
    }
}
