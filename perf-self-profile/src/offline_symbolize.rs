//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use dial9_trace_format::{
    decoder::{Decoder, StackPool},
    encoder::FxBuildHasher,
    types::{FieldValueRef, InternedString},
};
use std::collections::HashSet;
use std::io::{self, Write};

use crate::MapsEntry;

type FxHashSet<T> = HashSet<T, FxBuildHasher>;

/// Schema-based event for resolved symbol table entries.
///
/// Each entry maps an instruction pointer address to a resolved symbol name.
/// When a function has inlined callees, multiple entries share the same `addr`
/// with increasing `inline_depth` (0 = outermost).
#[derive(dial9_trace_format::TraceEvent)]
pub struct SymbolTableEntry {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub addr: u64,
    pub size: u64,
    pub symbol_name: InternedString,
    /// 0 = outermost function, 1+ = inlined callee depth.
    pub inline_depth: u64,
    /// Source file path from debug info (e.g. `/home/user/.cargo/registry/src/.../hyper-0.14.28/src/client.rs`).
    // TODO: consider splitting out source_file and source_dir to allow avoiding an extra allocation during interning.
    pub source_file: InternedString,
    /// Source line number, or 0 if unavailable.
    pub source_line: u64,
}

/// Symbolize a trace using caller-provided proc maps instead of reading them
/// from the trace.
///
/// Use this when the caller already has the memory mappings (e.g. from
/// `read_proc_maps()` in the same process). This avoids the overhead of
/// encoding proc maps into the trace and re-parsing them.
///
/// Each call constructs and tears down its own `blazesym::Symbolizer`. If
/// you are symbolizing a stream of segments, prefer
/// [`OfflineSymbolizer`] — it owns a long-lived symbolizer on a dedicated
/// thread so the per-segment cost drops once ELF/DWARF data is cached.
///
/// On non-Linux platforms this is a no-op (returns `Ok(())`).
pub fn symbolize_trace_with_maps(
    input: &[u8],
    maps: &[MapsEntry],
    output: &mut impl Write,
) -> io::Result<()> {
    let mut addresses: FxHashSet<u64> = FxHashSet::default();

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    decoder
        .for_each_event(|event| {
            collect_stack_frame_addresses(event.fields, event.stack_pool, &mut addresses);
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if addresses.is_empty() {
        return Ok(());
    }

    crate::sys::symbolize_one_shot(decoder, &addresses, maps, output)
}

fn collect_stack_frame_addresses(
    values: &[FieldValueRef<'_>],
    stack_pool: &StackPool,
    addresses: &mut FxHashSet<u64>,
) {
    for field in values {
        match field {
            FieldValueRef::StackFrames(frames) => {
                for addr in frames.iter() {
                    if addr != 0 {
                        addresses.insert(addr);
                    }
                }
            }
            FieldValueRef::PooledStackFrames(id) => {
                if let Some(frames) = stack_pool.get(*id) {
                    for &addr in frames {
                        if addr != 0 {
                            addresses.insert(addr);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// OfflineSymbolizer — long-lived symbolizer on a dedicated thread.
// ---------------------------------------------------------------------------

/// A long-lived symbolizer that owns a `blazesym::Symbolizer` on a
/// dedicated thread.
///
/// `blazesym::Symbolizer` is `!Send`: it cannot be moved between threads,
/// so it cannot be shared with tokio's blocking pool. At the same time,
/// throwing it away after each segment forces every flush to re-parse the
/// process's ELF/DWARF data — typically several hundred milliseconds (see
/// [#462](https://github.com/dial9-rs/dial9/issues/462)).
///
/// `OfflineSymbolizer` solves both problems:
///
/// - It owns one symbolizer for its lifetime, so blazesym's internal
///   per-source ELF cache stays warm across segments.
/// - It pins the symbolizer to a dedicated `std::thread` and exchanges
///   work via a channel, so callers from any thread can submit work.
///
/// [`symbolize`](Self::symbolize) is blocking: it sends a request and
/// waits for the response. The thread is kept alive even when an
/// individual symbolize request panics, so transient blazesym errors do
/// not destroy the cache.
///
/// On non-Linux platforms this type is a no-op shell — [`symbolize`]
/// always returns `Ok(Vec::new())` without spawning a thread.
///
/// # Example
///
/// ```no_run
/// use dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer;
/// use dial9_perf_self_profile::read_proc_maps;
/// # fn main() -> std::io::Result<()> {
/// let symbolizer = OfflineSymbolizer::new();
/// let maps = read_proc_maps();
/// let trace_bytes: &[u8] = b""; // your trace bytes
/// let symbols = symbolizer.symbolize(trace_bytes, &maps)?;
/// // `symbols` is the symbol-table data to append after `trace_bytes`.
/// # Ok(())
/// # }
/// ```
pub struct OfflineSymbolizer {
    inner: imp::OfflineSymbolizerImpl,
}

impl OfflineSymbolizer {
    /// Spawn a dedicated symbolizer thread.
    ///
    /// The thread is joined when this `OfflineSymbolizer` is dropped.
    ///
    /// On non-Linux platforms this returns a no-op shell that doesn't
    /// spawn anything.
    pub fn new() -> Self {
        Self {
            inner: imp::OfflineSymbolizerImpl::new(),
        }
    }

    /// Symbolize a trace segment.
    ///
    /// Blocks the calling thread until the dedicated symbolizer thread
    /// has finished. Returns the symbol-table bytes to append to the
    /// caller's trace data (matching the contract of
    /// [`symbolize_trace_with_maps`]).
    ///
    /// On non-Linux platforms this returns `Ok(Vec::new())`.
    pub fn symbolize(&self, input: &[u8], maps: &[MapsEntry]) -> io::Result<Vec<u8>> {
        self.inner.symbolize(input, maps)
    }

    /// Zero-copy variant of [`symbolize`](Self::symbolize) that accepts
    /// a [`Bytes`](bytes::Bytes) directly, avoiding a per-segment copy
    /// when the caller already holds reference-counted bytes (e.g. from
    /// a [`Payload`]).
    ///
    /// On non-Linux platforms this returns `Ok(Vec::new())`.
    pub fn symbolize_bytes(&self, input: bytes::Bytes, maps: &[MapsEntry]) -> io::Result<Vec<u8>> {
        self.inner.symbolize_bytes(input, maps)
    }

    /// Number of times this `OfflineSymbolizer`'s worker thread has
    /// constructed its underlying `blazesym::Symbolizer`.
    ///
    /// Should be `0` before the first [`symbolize`](Self::symbolize) call
    /// and `1` for every call thereafter. Tests use this to assert that
    /// the cache is being reused across segments without resorting to
    /// timing-based checks.
    ///
    /// On non-Linux platforms this always returns `0`.
    pub fn symbolizer_constructions(&self) -> u64 {
        self.inner.symbolizer_constructions()
    }

    /// Number of times this `OfflineSymbolizer`'s worker thread has
    /// constructed its [`SymbolizeState`] (reusable containers).
    ///
    /// Should be `0` before the first [`symbolize`](Self::symbolize) call
    /// and `1` for every call thereafter. Tests use this to assert that
    /// containers are reused across segments.
    ///
    /// On non-Linux platforms this always returns `0`.
    pub fn state_constructions(&self) -> u64 {
        self.inner.state_constructions()
    }
}

impl Default for OfflineSymbolizer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OfflineSymbolizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineSymbolizer")
            .field("symbolizer_constructions", &self.symbolizer_constructions())
            .finish_non_exhaustive()
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
mod imp {
    use super::{FxHashSet, MapsEntry, collect_stack_frame_addresses};
    use dial9_core::rate_limited;
    use dial9_trace_format::decoder::Decoder;
    use std::io;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// Reusable state for the symbolize worker thread, avoiding per-segment
    /// allocations. Containers are `.clear()`ed between requests but retain
    /// their capacity.
    struct SymbolizeState {
        addresses: FxHashSet<u64>,
        output: Vec<u8>,
        containers: crate::sys::SymbolizeContainers,
    }

    impl SymbolizeState {
        fn new() -> Self {
            Self {
                addresses: FxHashSet::default(),
                output: Vec::new(),
                containers: crate::sys::SymbolizeContainers::new(),
            }
        }

        fn clear(&mut self) {
            self.addresses.clear();
            self.output.clear();
            self.containers.clear();
        }
    }

    /// Work item submitted to the symbolizer thread.
    struct Request {
        input: bytes::Bytes,
        maps: Vec<MapsEntry>,
        respond_to: mpsc::SyncSender<io::Result<Vec<u8>>>,
    }

    /// Live thread state: channel handle and join handle. Held inside
    /// a `Mutex<Option<_>>` so the thread can be spawned lazily on the
    /// first `symbolize()` call.
    struct ThreadState {
        sender: mpsc::Sender<Request>,
        join: JoinHandle<()>,
    }

    /// Linux implementation: dedicated thread with one long-lived
    /// `blazesym::Symbolizer`. The thread is spawned **lazily** on the
    /// first call to [`symbolize`](Self::symbolize), not in
    /// [`new`](Self::new).
    ///
    /// This matters for CPU profiling: dial9 (and the underlying
    /// `perf_event_open`) only samples threads that descend from a
    /// thread which already has a tracked perf fd open. The runtime's
    /// `dial9-worker` thread is the one that calls `process()` on us;
    /// by deferring the spawn until that first call, the symbolizer
    /// thread becomes a child of `dial9-worker` and inherits its perf
    /// events. If we spawned in `new()`, the symbolizer thread would
    /// be a sibling of `dial9-worker` and CPU samples from it would
    /// never appear in the trace.
    pub(super) struct OfflineSymbolizerImpl {
        state: Mutex<Option<ThreadState>>,
        symbolizer_constructions: Arc<AtomicU64>,
        state_constructions: Arc<AtomicU64>,
    }

    impl OfflineSymbolizerImpl {
        pub(super) fn new() -> Self {
            Self {
                state: Mutex::new(None),
                symbolizer_constructions: Arc::new(AtomicU64::new(0)),
                state_constructions: Arc::new(AtomicU64::new(0)),
            }
        }

        /// Lazily spawn the worker thread on first use. Subsequent calls
        /// reuse the same thread.
        fn ensure_thread(&self) -> io::Result<mpsc::Sender<Request>> {
            let mut guard = self.state.lock().unwrap();
            if let Some(state) = guard.as_ref() {
                return Ok(state.sender.clone());
            }
            let (tx, rx) = mpsc::channel::<Request>();
            let counter = Arc::clone(&self.symbolizer_constructions);
            let state_counter = Arc::clone(&self.state_constructions);
            let join = std::thread::Builder::new()
                .name("dial9-symbolizer".to_string())
                .spawn(move || worker_loop(rx, counter, state_counter))
                .map_err(|e| {
                    io::Error::other(format!("failed to spawn dial9-symbolizer thread: {e}"))
                })?;
            *guard = Some(ThreadState {
                sender: tx.clone(),
                join,
            });
            Ok(tx)
        }

        pub(super) fn symbolize(&self, input: &[u8], maps: &[MapsEntry]) -> io::Result<Vec<u8>> {
            self.symbolize_bytes(bytes::Bytes::copy_from_slice(input), maps)
        }

        pub(super) fn symbolize_bytes(
            &self,
            input: bytes::Bytes,
            maps: &[MapsEntry],
        ) -> io::Result<Vec<u8>> {
            let sender = self.ensure_thread()?;
            // sync_channel(1) gives us a bounded response channel — we
            // never queue responses; one in flight at a time.
            let (resp_tx, resp_rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(1);
            let req = Request {
                input,
                maps: maps.to_vec(),
                respond_to: resp_tx,
            };
            sender
                .send(req)
                .map_err(|_| io::Error::other("symbolizer thread is no longer running"))?;
            resp_rx
                .recv()
                .map_err(|_| io::Error::other("symbolizer thread dropped response channel"))?
        }

        pub(super) fn symbolizer_constructions(&self) -> u64 {
            self.symbolizer_constructions.load(Ordering::Relaxed)
        }

        pub(super) fn state_constructions(&self) -> u64 {
            self.state_constructions.load(Ordering::Relaxed)
        }
    }

    impl Drop for OfflineSymbolizerImpl {
        fn drop(&mut self) {
            // Closing the sender lets the worker loop exit on `recv()`.
            // If the thread was never spawned, there is nothing to do.
            let state = self.state.lock().unwrap().take();
            if let Some(state) = state {
                drop(state.sender);
                let _ = state.join.join();
            }
        }
    }

    fn worker_loop(
        rx: mpsc::Receiver<Request>,
        constructions: Arc<AtomicU64>,
        state_constructions: Arc<AtomicU64>,
    ) {
        // Register with ctimer so this thread is profiled even without perf's
        // inherit mode.
        let _ = crate::register_current_thread();

        // Lazy: only construct the Symbolizer when we receive the first
        // request, so the cost is paid on first segment, not on creation.
        let mut symbolizer: Option<blazesym::symbolize::Symbolizer> = None;
        let mut state = SymbolizeState::new();
        state_constructions.fetch_add(1, Ordering::Relaxed);

        while let Ok(req) = rx.recv() {
            let sym = symbolizer.get_or_insert_with(|| {
                constructions.fetch_add(1, Ordering::Relaxed);
                blazesym::symbolize::Symbolizer::new()
            });
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run_symbolize(&req.input, &req.maps, sym, &mut state)
            }));
            let result = match result {
                Ok(r) => r,
                Err(payload) => {
                    let msg = panic_message(&payload);
                    rate_limited!(Duration::from_secs(60), {
                        tracing::error!(
                            target: "dial9_offline_symbolizer",
                            panic = %msg,
                            "symbolize panicked, dropping segment but keeping symbolizer thread alive"
                        );
                    });
                    Err(io::Error::other(format!("symbolize panicked: {msg}")))
                }
            };
            // Caller may have dropped the receiver (timed out, gave up).
            // That's fine — just skip this response.
            let _ = req.respond_to.send(result);
        }
        crate::unregister_current_thread();
    }

    fn run_symbolize(
        input: &[u8],
        maps: &[MapsEntry],
        symbolizer: &blazesym::symbolize::Symbolizer,
        state: &mut SymbolizeState,
    ) -> io::Result<Vec<u8>> {
        state.clear();

        let mut decoder = Decoder::new(input)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

        decoder
            .for_each_event(|event| {
                collect_stack_frame_addresses(event.fields, event.stack_pool, &mut state.addresses);
            })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        if state.addresses.is_empty() {
            return Ok(std::mem::take(&mut state.output));
        }

        crate::sys::write_symbol_data(
            decoder,
            &state.addresses,
            maps,
            symbolizer,
            &mut state.containers,
            &mut state.output,
        )?;
        Ok(std::mem::take(&mut state.output))
    }

    fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
mod imp {
    use super::MapsEntry;
    use std::io;

    /// No-op stub for non-Linux platforms — mirrors the behaviour of
    /// [`symbolize_trace_with_maps`](super::symbolize_trace_with_maps),
    /// which is also a no-op there.
    pub(super) struct OfflineSymbolizerImpl;

    impl OfflineSymbolizerImpl {
        pub(super) fn new() -> Self {
            Self
        }

        pub(super) fn symbolize(&self, _input: &[u8], _maps: &[MapsEntry]) -> io::Result<Vec<u8>> {
            Ok(Vec::new())
        }

        pub(super) fn symbolize_bytes(
            &self,
            _input: bytes::Bytes,
            _maps: &[MapsEntry],
        ) -> io::Result<Vec<u8>> {
            Ok(Vec::new())
        }

        pub(super) fn symbolizer_constructions(&self) -> u64 {
            0
        }

        pub(super) fn state_constructions(&self) -> u64 {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::{
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
        schema::FieldDef,
        types::{FieldType, FieldValue},
    };

    #[test]
    fn symbol_table_event_round_trip() {
        let mut enc = Encoder::new();
        let sym_name = enc.intern_string("my_function").unwrap();
        let src_file = enc.intern_string("/src/lib.rs").unwrap();
        enc.write(&SymbolTableEntry {
            timestamp_ns: 0,
            addr: 0x1000,
            size: 256,
            symbol_name: sym_name,
            inline_depth: 0,
            source_file: src_file,
            source_line: 42,
        })
        .unwrap();
        let buf = enc.finish();

        let mut dec = Decoder::new(&buf).unwrap();
        let frames = dec.decode_all();
        // StringPool("my_function") + StringPool("/src/lib.rs") + Schema + Event
        assert_eq!(frames.len(), 4);
        if let DecodedFrame::Event { values, .. } = &frames[3] {
            assert_eq!(values[0], FieldValue::Varint(0x1000));
            assert_eq!(values[1], FieldValue::Varint(256));
            assert_eq!(
                values[2],
                FieldValue::PooledString(InternedString::from_raw(0))
            );
            assert_eq!(values[3], FieldValue::Varint(0));
            assert_eq!(
                values[4],
                FieldValue::PooledString(InternedString::from_raw(1))
            );
            assert_eq!(values[5], FieldValue::Varint(42));
        } else {
            panic!("expected event frame");
        }
        assert_eq!(
            dec.string_pool().get(InternedString::from_raw(0)),
            Some("my_function")
        );
    }

    #[test]
    fn symbolize_empty_trace_writes_nothing() {
        let buf = Encoder::new().finish();
        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &[], &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_no_stack_frames_writes_nothing() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema("Ev", vec![FieldDef::new("count", FieldType::Varint)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(0), FieldValue::Varint(42)])
            .unwrap();
        let buf = enc.finish();

        let maps = vec![MapsEntry {
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            path: "/bin/test".into(),
        }];
        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &maps, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_empty_maps_writes_nothing() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(0),
                FieldValue::StackFrames(vec![0x1000].into()),
            ],
        )
        .unwrap();
        let buf = enc.finish();

        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &[], &mut output).unwrap();
        // Addresses exist but none match any mapping, so no symbols are emitted.
        assert!(output.is_empty());
    }

    /// A single [`OfflineSymbolizer`] must reuse one underlying
    /// `blazesym::Symbolizer` across many segments. Otherwise we re-parse
    /// every ELF/DWARF on every flush, which is the 300 ms stall described
    /// in #462.
    ///
    /// We assert the invariant directly via the per-instance counter
    /// `OfflineSymbolizer::symbolizer_constructions()`. The counter is
    /// scoped to the instance, so this test is parallel-safe.
    #[cfg(target_os = "linux")]
    #[test]
    fn offline_symbolizer_reuses_one_blazesym_across_segments() {
        let addr = offline_symbolizer_reuses_one_blazesym_across_segments as *const () as u64;
        let raw_maps = crate::read_proc_maps();

        // Build three independent segments, each pointing at our own function
        // address. Symbolizing each requires walking the same set of ELF
        // mappings — so a non-cached symbolizer would reparse them three times.
        let mut segs: Vec<Vec<u8>> = Vec::with_capacity(3);
        for _ in 0..3 {
            let mut enc = Encoder::new();
            let schema = enc
                .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
                .unwrap();
            enc.write_event(
                &schema,
                &[
                    FieldValue::Varint(0),
                    FieldValue::StackFrames(vec![addr].into()),
                ],
            )
            .unwrap();
            segs.push(enc.finish());
        }

        let symbolizer = OfflineSymbolizer::new();
        assert_eq!(
            symbolizer.symbolizer_constructions(),
            0,
            "construction is lazy: nothing should happen until the first symbolize call",
        );
        for seg in &segs {
            let out = symbolizer.symbolize(seg, &raw_maps).unwrap();
            assert!(!out.is_empty(), "every segment must produce a symbol table");
        }
        assert_eq!(
            symbolizer.symbolizer_constructions(),
            1,
            "OfflineSymbolizer must construct exactly one blazesym::Symbolizer",
        );
    }

    /// The [`OfflineSymbolizer`] must reuse its internal `SymbolizeState`
    /// (address set, output buffer, containers) across segments rather than
    /// allocating fresh containers per call.
    #[cfg(target_os = "linux")]
    #[test]
    fn offline_symbolizer_reuses_state_across_segments() {
        let addr = offline_symbolizer_reuses_state_across_segments as *const () as u64;
        let raw_maps = crate::read_proc_maps();

        let mut segs: Vec<Vec<u8>> = Vec::with_capacity(3);
        for _ in 0..3 {
            let mut enc = Encoder::new();
            let schema = enc
                .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
                .unwrap();
            enc.write_event(
                &schema,
                &[
                    FieldValue::Varint(0),
                    FieldValue::StackFrames(vec![addr].into()),
                ],
            )
            .unwrap();
            segs.push(enc.finish());
        }

        let symbolizer = OfflineSymbolizer::new();
        assert_eq!(symbolizer.state_constructions(), 0);
        for seg in &segs {
            let out = symbolizer.symbolize(seg, &raw_maps).unwrap();
            assert!(!out.is_empty());
        }
        assert_eq!(
            symbolizer.state_constructions(),
            1,
            "SymbolizeState must be constructed exactly once and reused",
        );
    }

    /// [`OfflineSymbolizer::symbolize_bytes`] produces the same output as
    /// [`OfflineSymbolizer::symbolize`] but accepts `Bytes` directly,
    /// avoiding a per-segment copy when the caller already holds
    /// reference-counted bytes.
    #[cfg(target_os = "linux")]
    #[test]
    fn symbolize_bytes_matches_symbolize_slice() {
        let addr = symbolize_bytes_matches_symbolize_slice as *const () as u64;
        let raw_maps = crate::read_proc_maps();

        let mut enc = Encoder::new();
        let schema = enc
            .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(0),
                FieldValue::StackFrames(vec![addr].into()),
            ],
        )
        .unwrap();
        let buf = enc.finish();

        let symbolizer = OfflineSymbolizer::new();
        let out_slice = symbolizer.symbolize(&buf, &raw_maps).unwrap();
        let out_bytes = symbolizer
            .symbolize_bytes(bytes::Bytes::from(buf.clone()), &raw_maps)
            .unwrap();
        assert_eq!(out_slice, out_bytes);
        assert!(!out_slice.is_empty());
    }
}
