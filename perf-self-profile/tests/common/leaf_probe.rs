//! Test helper: detects whether this binary gives *leaf* functions a frame
//! record. Integration tests are built with the same flags as the code under
//! test, so this probe's prologue reflects the build being tested.
//!
//! The recursion self-test (`Unwinder::self_test_frame_pointers`) cannot: its helper
//! makes a call, so `-C force-frame-pointers=non-leaf` gives it a frame record
//! and the self-test passes while leaves stay frameless. That is the aarch64
//! default, so omitting `-C force-frame-pointers=yes` lands there.
//!
//! Read the answer off our own codegen rather than inferring the build mode:
//! check a known leaf's prologue for the frame-pointer instruction.

/// Guaranteed leaf: no calls, `wrapping_mul` cannot panic, and
/// `#[inline(never)]` keeps it a real function with a prologue. Built with the
/// user's flags, so its prologue reflects their `force-frame-pointers` setting.
#[inline(never)]
extern "C" fn probe_leaf(x: u64) -> u64 {
    x.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Instructions scanned from the probe's entry. Enough to skip a
/// `bti`/`paciasp`/`endbr64` preamble and a stack-pointer adjustment.
const SCAN_WORDS: usize = 8;

/// Whether leaf functions in this binary carry a frame record. `None` on an
/// unrecognised architecture.
pub fn leaf_frame_pointers_present() -> Option<bool> {
    // Keep the probe from being optimised away entirely.
    std::hint::black_box(probe_leaf(std::hint::black_box(1)));
    let entry = probe_leaf as *const ();
    // SAFETY: `entry` is the address of a function in our own executable text,
    // which is mapped readable for the life of the process. We read at most
    // SCAN_WORDS * 4 bytes forward from a function that has at least that many
    // bytes of prologue + body + ret, using unaligned reads.
    unsafe { decode_prologue(entry) }
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn decode_prologue(entry: *const ()) -> Option<bool> {
    let words = entry as *const u32;
    for i in 0..SCAN_WORDS {
        // SAFETY: see caller; reading within the probe function's own code.
        let insn = unsafe { words.add(i).read_unaligned() };
        if establishes_frame_pointer_aarch64(insn) {
            return Some(true);
        }
        if insn == RET {
            // Reached the end of the probe without a frame pointer.
            return Some(false);
        }
    }
    Some(false)
}

#[cfg(target_arch = "aarch64")]
pub const RET: u32 = 0xD65F_03C0;

/// `ADD x29, sp, #imm`: how both AAPCS64 prologue shapes set the frame
/// pointer, including `mov x29, sp` (`0x910003FD`).
#[cfg(target_arch = "aarch64")]
pub fn establishes_frame_pointer_aarch64(insn: u32) -> bool {
    const ADD_IMM64_MASK: u32 = 0xFF80_0000;
    const ADD_IMM64: u32 = 0x9100_0000;
    let rd = insn & 0x1F;
    let rn = (insn >> 5) & 0x1F;
    insn & ADD_IMM64_MASK == ADD_IMM64 && rd == 29 && rn == 31
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn decode_prologue(entry: *const ()) -> Option<bool> {
    let bytes = entry as *const u8;
    // `mov %rsp, %rbp` is `48 89 e5`, possibly after `endbr64`.
    for i in 0..SCAN_WORDS * 4 {
        // SAFETY: see caller; reading within the probe function's own code.
        let b0 = unsafe { bytes.add(i).read_unaligned() };
        if b0 == 0xC3 {
            // `ret` before any frame-pointer setup.
            return Some(false);
        }
        if i + 2 < SCAN_WORDS * 4 {
            let b1 = unsafe { bytes.add(i + 1).read_unaligned() };
            let b2 = unsafe { bytes.add(i + 2).read_unaligned() };
            if (b0, b1, b2) == (0x48, 0x89, 0xE5) {
                return Some(true);
            }
        }
    }
    Some(false)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn decode_prologue(_entry: *const ()) -> Option<bool> {
    None
}
