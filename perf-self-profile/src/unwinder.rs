//! Framepointer-based stack unwinding
//!
//! This implementation uses a SIGSEGV fault handler to allow safe walking of stacks. Without
//! this, there is no way to perform framepointer unwinding without risking segfaults when walking
//! stacks where framepointers are not enabled.
//!
//! Because of this, the unwinder must be "[`install`ed](Unwinder::install)" before
//! you can use it to [`capture`](Unwinder::capture) a stack.
//!
//! The unwound stacks are only addresses. You must use a symbolizer separately to
//! convert the addresses into function names.

/// Result of a [`Unwinder::capture`] call.
///
/// The captured program counters are written into the output buffer supplied
/// to `capture`; this struct describes the metadata of the capture.
///
/// `#[non_exhaustive]` so new fields can be added without breaking callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureResult {
    /// Number of frames written into the caller's output buffer.
    ///
    /// Frames `out[0..frames_written]` are valid.
    pub frames_written: usize,
    /// `true` if the walk stopped because the output buffer (or the
    /// internal `MAX_FRAMES` cap of 128) was full while at least one
    /// additional frame was still walkable. When `true`, the outer
    /// frames of the stack (closer to `main`) have been dropped.
    pub truncated: bool,
}

/// Handle that proves the SIGSEGV fault handler is installed.
///
/// This type is zero-sized and can be freely copied and cloned.
#[derive(Clone, Copy, Debug)]
pub struct Unwinder {
    _private: (),
}

impl Unwinder {
    /// Install the SIGSEGV fault handler used by stack capture.
    /// Idempotent: safe to call multiple times from multiple threads.
    ///
    /// Returns `Err` if `sigaction` fails (Linux) or if the platform is
    /// unsupported.
    ///
    /// Frame pointers are required for complete stacks (build with
    /// `-C force-frame-pointers=yes`). Without them, capture safely returns an
    /// empty or shallow stack.
    pub fn install() -> std::io::Result<Self> {
        platform::install()?;
        Ok(Self { _private: () })
    }

    /// Verify that our SIGSEGV handler is still the active handler for
    /// SIGSEGV on this process. Returns `true` if the handler we installed
    /// is still registered.
    ///
    /// Another library or the runtime may install its own SIGSEGV handler
    /// after [`install`](Self::install) is called. If that handler does not
    /// chain to ours, [`capture`](Self::capture) may crash on a bad frame
    /// pointer instead of aborting the walk safely. Callers who need
    /// defence against this can call `verify_handler` periodically or
    /// before safety-critical captures.
    ///
    /// Performs one `sigaction` syscall. Not suitable for per-sample hot
    /// paths.
    pub fn verify_handler(&self) -> bool {
        platform::verify_handler()
    }

    /// Capture a stack trace of the calling thread into `out`. Returns a
    /// [`CaptureResult`] describing the number of frames written and
    /// whether the walk was truncated. Never allocates.
    ///
    /// # Frame-0 contract
    /// With frame pointers enabled, `out[0]` is the return address of `capture`
    /// itself — i.e. a PC *inside the caller of `capture`*. Subsequent frames
    /// walk outward via the frame-pointer chain. Any `#[inline(never)]` shim
    /// inserted between the user's code and `capture` will appear as an extra
    /// frame.
    ///
    /// Without frame pointers, capture is crash-safe but best-effort: output
    /// may be empty or shallow, and frame 0 is not guaranteed to identify the
    /// direct caller.
    ///
    /// # Buffer and truncation
    /// At most `out.len().min(MAX_FRAMES)` frames are written (where
    /// `MAX_FRAMES = 128`). If the real stack is deeper, innermost frames
    /// are kept and outer frames are dropped; `CaptureResult::truncated`
    /// is set to `true`.
    ///
    /// If the current frame record is implausible or unreadable, capture
    /// returns no frames. If only the caller's frame pointer is unusable,
    /// capture returns frame 0 and stops. This lets binaries without frame
    /// pointers degrade to an empty or shallow stack instead of crashing,
    /// although arbitrary register values can occasionally resemble a valid
    /// frame record.
    ///
    /// # Safety
    /// - [`install`](Self::install) must have succeeded and the SIGSEGV
    ///   handler it registered must still be active. If another library
    ///   has replaced the SIGSEGV handler without chaining to ours, a
    ///   faulty frame-pointer chain can crash the process instead of
    ///   being caught. Use [`verify_handler`](Self::verify_handler) if
    ///   you need to defend against third-party signal handler
    ///   installation.
    /// - Must not be called from inside a signal handler for SIGSEGV
    ///   (that would recurse into our own handler without bound).
    // Takes `&self` to prove that `Unwinder::install()` succeeded, even though
    // no instance data is accessed internally.
    #[inline(never)]
    pub unsafe fn capture(&self, out: &mut [u64]) -> CaptureResult {
        // Debug-only check that our SIGSEGV handler is still the active
        // one. In release builds this is skipped to keep `capture` syscall-free
        // on the hot path; callers who need this at runtime should use
        // [`verify_handler`](Self::verify_handler) explicitly.
        debug_assert!(
            self.verify_handler(),
            "Unwinder::capture called but our SIGSEGV handler is no longer active; \
             something replaced it without chaining. See Unwinder::verify_handler."
        );
        // SAFETY: forwarding Unwinder::capture's own safety contract to
        // platform::capture (handler installed, not in a SIGSEGV handler).
        unsafe { platform::capture(out) }
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    use super::CaptureResult;
    use crate::sys::fp_profiler::{
        SAFE_LOAD_FAULT, handler_is_installed, install_handler, load,
        unwind::{DEAD_ZONE, MAX_FRAME_SIZE, strip_pac, unwind},
    };

    pub fn install() -> std::io::Result<()> {
        // SAFETY: installs the SIGSEGV handler for safe_load; idempotent.
        unsafe { install_handler() }
    }

    pub fn verify_handler() -> bool {
        handler_is_installed()
    }

    /// Called from [`Unwinder::capture`] (which is `#[inline(never)]`), so the
    /// "current frame" observed here is `Unwinder::capture`'s frame. This
    /// function is `#[inline(always)]` specifically so it does *not* introduce
    /// another frame — see the `frame_zero_points_into_caller_of_capture`
    /// test.
    ///
    /// # Safety
    /// Same obligations as [`Unwinder::capture`]: handler installed and not
    /// inside a SIGSEGV handler.
    #[inline(always)]
    pub unsafe fn capture(out: &mut [u64]) -> CaptureResult {
        // SAFETY: called from Unwinder::capture which forwards the safety
        // contract (handler installed and not inside a SIGSEGV handler).
        let Some((pc, fp, sp)) = (unsafe { read_caller_regs() }) else {
            return CaptureResult {
                frames_written: 0,
                truncated: false,
            };
        };
        // SAFETY: handler is installed (caller holds Unwinder), and we are
        // not inside a SIGSEGV handler (see Unwinder::capture safety
        // contract).
        unsafe { unwind(pc, fp, sp, out) }
    }

    /// Read `(pc, fp, sp)` such that `pc` is the PC to use for frame 0 and
    /// `fp` is the saved frame pointer of the frame *above* the current one.
    ///
    /// Because this is `#[inline(always)]`, the `rbp`/`x29` read observes
    /// `Unwinder::capture`'s frame (its one and only non-inlined ancestor).
    /// - `*(rbp + 8)` on x86_64 / LR save slot on aarch64 is
    ///   `Unwinder::capture`'s return address → goes to `out[0]`.
    /// - `*rbp` / `*x29` is the saved fp of the caller of `Unwinder::capture`
    ///   → where we start walking the chain.
    ///
    /// Returns `None` when the current frame record is implausible or its
    /// return-address slot cannot be read safely. An unusable caller frame
    /// pointer is returned as zero so the unwind retains frame 0 and stops.
    ///
    /// # Safety
    /// - Must be called with `#[inline(always)]` preserved so the register
    ///   read observes `Unwinder::capture`'s frame, not this helper's. If
    ///   inlined into a different caller or promoted to a standalone frame,
    ///   the returned `fp`/return-address semantics change.
    /// - Must only be called after [`install_handler`] has succeeded and not
    ///   from inside a SIGSEGV handler.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn read_caller_regs() -> Option<(usize, usize, usize)> {
        let fp: usize;
        let sp: usize;
        // SAFETY: Reading `rbp`/`rsp` with `nostack, nomem` has no memory
        // side effects and cannot invalidate Rust's stack invariants; we
        // do not modify either register.
        unsafe {
            core::arch::asm!(
                "mov {fp}, rbp",
                "mov {sp}, rsp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        // SAFETY: the caller guarantees the safe-load handler is active.
        unsafe { read_frame_record(fp, sp) }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn read_caller_regs() -> Option<(usize, usize, usize)> {
        let fp: usize;
        let sp: usize;
        // SAFETY: Reading `x29`/`sp` with `nostack, nomem` has no memory
        // side effects and cannot invalidate Rust's stack invariants; we
        // do not modify either register.
        unsafe {
            core::arch::asm!(
                "mov {fp}, x29",
                "mov {sp}, sp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        // SAFETY: the caller guarantees the safe-load handler is active.
        unsafe { read_frame_record(fp, sp) }
    }

    #[inline(always)]
    unsafe fn read_frame_record(fp: usize, sp: usize) -> Option<(usize, usize, usize)> {
        let word = core::mem::size_of::<usize>();
        if fp < sp || fp - sp > MAX_FRAME_SIZE || fp & (word - 1) != 0 {
            return None;
        }

        let ret_addr_slot = fp.checked_add(word)?;
        // SAFETY: the slot is aligned and the caller guarantees the
        // safe-load handler is active.
        let ret_addr = strip_pac(unsafe { load(ret_addr_slot as *const usize) });
        if ret_addr == SAFE_LOAD_FAULT || !(DEAD_ZONE..=usize::MAX - DEAD_ZONE).contains(&ret_addr)
        {
            return None;
        }

        // SAFETY: `fp` is aligned and the safe-load handler is active.
        let caller_fp = unsafe { load(fp as *const usize) };
        let caller_fp =
            if caller_fp != SAFE_LOAD_FAULT && caller_fp > fp && caller_fp - fp <= MAX_FRAME_SIZE {
                caller_fp
            } else {
                0
            };

        Some((ret_addr, caller_fp, sp))
    }

    #[cfg(test)]
    #[allow(unused_assignments)] // frame records are read through safe_load assembly
    mod tests {
        use super::*;

        fn install() {
            // SAFETY: tests install the process-global handler idempotently.
            unsafe { install_handler().unwrap() };
        }

        fn page_size() -> usize {
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            assert!(page_size > 0);
            page_size as usize
        }

        #[test]
        fn valid_frame_record_is_read() {
            install();
            let mut frame = [0usize; 2];
            let fp = frame.as_mut_ptr() as usize;
            let caller_fp = fp + 2 * core::mem::size_of::<usize>();
            let ret_addr = DEAD_ZONE + 1;
            frame[0] = caller_fp;
            frame[1] = ret_addr;

            let result = unsafe { read_frame_record(fp, fp) };

            assert_eq!(result, Some((ret_addr, caller_fp, fp)));
        }

        #[test]
        fn implausible_frame_pointer_degrades_to_no_frames() {
            install();
            let word = core::mem::size_of::<usize>();
            let sp = 0x10_000usize;
            let aligned_max = usize::MAX & !(word - 1);
            let cases = [
                (sp - word, sp, "below stack pointer"),
                (
                    sp + MAX_FRAME_SIZE + word,
                    sp,
                    "too far above stack pointer",
                ),
                (sp + 1, sp, "misaligned"),
                (aligned_max, aligned_max, "return-address slot overflows"),
            ];

            for (fp, sp, case) in cases {
                let result = unsafe { read_frame_record(fp, sp) };
                assert_eq!(result, None, "{case}");
            }
        }

        #[test]
        fn implausible_return_address_degrades_to_no_frames() {
            install();
            let mut frame = [0usize; 2];
            let fp = frame.as_mut_ptr() as usize;
            frame[0] = fp + 2 * core::mem::size_of::<usize>();

            for ret_addr in [SAFE_LOAD_FAULT, DEAD_ZONE - 1] {
                frame[1] = ret_addr;
                let result = unsafe { read_frame_record(fp, fp) };
                assert_eq!(result, None, "return address {ret_addr:#x}");
            }
        }

        #[test]
        fn implausible_caller_frame_pointer_retains_frame_zero() {
            install();
            let mut frame = [0usize; 2];
            let fp = frame.as_mut_ptr() as usize;
            let ret_addr = DEAD_ZONE + 1;
            frame[1] = ret_addr;

            for caller_fp in [fp, fp + MAX_FRAME_SIZE + core::mem::size_of::<usize>()] {
                frame[0] = caller_fp;
                let result = unsafe { read_frame_record(fp, fp) };
                assert_eq!(result, Some((ret_addr, 0, fp)));
            }
        }

        #[test]
        fn unreadable_return_address_degrades_to_no_frames() {
            install();
            let page_size = page_size();
            let page = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    page_size,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(page, libc::MAP_FAILED);

            let result = unsafe { read_frame_record(page as usize, page as usize) };

            assert_eq!(unsafe { libc::munmap(page, page_size) }, 0);
            assert_eq!(result, None);
        }

        #[test]
        fn unreadable_caller_fp_retains_frame_zero() {
            install();
            let page_size = page_size();
            let region = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    2 * page_size,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(region, libc::MAP_FAILED);

            let second_page = unsafe { region.cast::<u8>().add(page_size) };
            assert_eq!(
                unsafe {
                    libc::mprotect(
                        second_page.cast(),
                        page_size,
                        libc::PROT_READ | libc::PROT_WRITE,
                    )
                },
                0
            );
            let ret_addr = DEAD_ZONE + 1;
            unsafe { second_page.cast::<usize>().write(ret_addr) };
            let fp = second_page as usize - core::mem::size_of::<usize>();

            let result = unsafe { read_frame_record(fp, fp) };

            assert_eq!(unsafe { libc::munmap(region, 2 * page_size) }, 0);
            assert_eq!(result, Some((ret_addr, 0, fp)));
        }
    }
}

#[cfg(not(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use super::CaptureResult;

    pub fn install() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unwinder is only available on Linux x86_64/aarch64",
        ))
    }

    pub fn verify_handler() -> bool {
        false
    }

    pub unsafe fn capture(_out: &mut [u64]) -> CaptureResult {
        CaptureResult {
            frames_written: 0,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    mod linux {
        use super::super::*;

        /// Skip the test if something (e.g. ASAN) replaced our SIGSEGV handler
        /// after install. Returns the Unwinder on success.
        fn install_or_skip() -> Option<Unwinder> {
            let u = Unwinder::install().unwrap();
            if !u.verify_handler() {
                eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
                return None;
            }
            Some(u)
        }

        #[test]
        fn install_is_idempotent() {
            let r1 = Unwinder::install();
            let r2 = Unwinder::install();
            let r3 = Unwinder::install();
            assert!(r1.is_ok());
            assert!(r2.is_ok());
            assert!(r3.is_ok());
        }

        #[test]
        fn install_is_idempotent_across_threads() {
            let handles: Vec<_> = (0..8)
                .map(|_| std::thread::spawn(Unwinder::install))
                .collect();
            for h in handles {
                assert!(h.join().unwrap().is_ok());
            }
        }

        #[test]
        fn verify_handler_true_after_install() {
            let Some(u) = install_or_skip() else {
                return;
            };
            assert!(u.verify_handler(), "handler should be active after install");
        }

        #[test]
        fn capture_produces_frames() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            #[inline(never)]
            fn helper(u: &Unwinder) -> (CaptureResult, [u64; 64]) {
                let mut out = [0u64; 64];
                // SAFETY: handler installed via Unwinder::install above; test thread
                // is not inside a signal handler.
                let result = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                (result, out)
            }
            let (result, out) = helper(&unwinder);
            assert!(
                result.frames_written >= 2,
                "expected at least 2 frames, got {}",
                result.frames_written
            );
            for (i, &addr) in out.iter().enumerate().take(result.frames_written) {
                assert_ne!(addr, 0, "frame {i} must be non-zero");
            }
        }

        /// Tighter version of the frame-0 contract test: verify that frame 0
        /// lands inside `helper` (the caller of `capture`) rather than inside
        /// `Unwinder::capture` itself. This catches the bug where the old
        /// double-`#[inline(never)]` layering made frame 0 point at an
        /// instruction inside `Unwinder::capture`'s body.
        ///
        /// We check the contract by *symbolizing* frame 0 rather than
        /// comparing it against `helper as *const ()` plus a byte window.
        /// A function pointer is only the symbol's entry address; codegen is
        /// free to place basic blocks (and cold/split fragments) below that
        /// entry, so a captured return address can legitimately land *before*
        /// `helper as *const ()`. Earlier window-based versions of this test
        /// were flaky for exactly that reason under different toolchains. The
        /// symbol name is the layout-independent ground truth.
        #[test]
        fn frame_zero_points_into_caller_of_capture() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };

            #[inline(never)]
            fn helper(u: &Unwinder) -> u64 {
                let mut out = [0u64; 64];
                // SAFETY: same as capture_produces_frames.
                let result = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                assert!(result.frames_written >= 1);
                out[0]
            }

            let frame0 = helper(&unwinder);
            let name = crate::resolve_symbol(frame0).name;
            let Some(name) = name else {
                // Without symbols (e.g. a stripped test binary) there is
                // nothing to assert against; the address-non-zero contract is
                // already covered by `capture_produces_frames`.
                eprintln!("skipping: frame 0 {frame0:#x} did not symbolize");
                return;
            };

            // Frame 0 is the return address of `capture`, i.e. a PC inside
            // `helper`. It must resolve to `helper` and in particular must NOT
            // resolve to `Unwinder::capture` (the old inlining bug). Match on
            // the trailing path segment: the enclosing test function name
            // itself contains "capture", so a substring check would be
            // ambiguous, but the leaf symbol is `…::helper` vs `…::capture`.
            let leaf = name.rsplit("::").next().unwrap_or(&name);
            assert_eq!(
                leaf, "helper",
                "frame 0 {frame0:#x} should symbolize to `helper`, got {name:?}",
            );
        }

        #[test]
        fn capture_respects_output_buffer_limit() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            let mut out = [0u64; 1];
            // SAFETY: handler installed; test context is not a signal handler.
            let result = unsafe { unwinder.capture(&mut out) };
            assert!(
                result.frames_written <= 1,
                "expected at most 1 frame, got {}",
                result.frames_written
            );
            if result.frames_written == 1 {
                assert_ne!(out[0], 0, "frame 0 must be non-zero when written");
            }
        }

        #[test]
        fn capture_reports_truncation_with_tiny_buffer() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            // Build a small but real call chain so a 1-slot buffer is bound
            // to truncate.
            #[inline(never)]
            fn depth_2(u: &Unwinder) -> CaptureResult {
                let mut out = [0u64; 1];
                // SAFETY: handler installed above.
                let r = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                r
            }
            #[inline(never)]
            fn depth_1(u: &Unwinder) -> CaptureResult {
                std::hint::black_box(depth_2(u))
            }
            let result = depth_1(&unwinder);
            assert_eq!(result.frames_written, 1);
            assert!(
                result.truncated,
                "a 1-slot buffer with a multi-frame stack must report truncated"
            );
        }

        #[test]
        fn capture_with_empty_buffer_reports_truncation() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            // SAFETY: handler installed above.
            let result = unsafe { unwinder.capture(&mut []) };
            assert_eq!(result.frames_written, 0);
            assert!(result.truncated);
        }
    }

    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    mod unsupported {
        use super::super::*;

        #[test]
        fn install_returns_unsupported() {
            let err = Unwinder::install().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        }
    }
}
