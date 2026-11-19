//! Detects whether this binary gives *leaf* functions a frame record.
//!
//! The recursion self-test in [`crate::unwinder`] cannot: its `recurse` helper
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
pub(crate) fn leaf_frame_pointers_present() -> Option<bool> {
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
unsafe fn decode_prologue(entry: *const ()) -> Option<bool> {
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
const RET: u32 = 0xD65F_03C0;

/// `ADD x29, sp, #imm`: how both AAPCS64 prologue shapes set the frame
/// pointer, including `mov x29, sp` (`0x910003FD`).
#[cfg(target_arch = "aarch64")]
fn establishes_frame_pointer_aarch64(insn: u32) -> bool {
    const ADD_IMM64_MASK: u32 = 0xFF80_0000;
    const ADD_IMM64: u32 = 0x9100_0000;
    let rd = insn & 0x1F;
    let rn = (insn >> 5) & 0x1F;
    insn & ADD_IMM64_MASK == ADD_IMM64 && rd == 29 && rn == 31
}

#[cfg(target_arch = "x86_64")]
unsafe fn decode_prologue(entry: *const ()) -> Option<bool> {
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
unsafe fn decode_prologue(_entry: *const ()) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the architectures we decode, the answer must be conclusive.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn detection_is_conclusive_on_supported_arch() {
        assert!(leaf_frame_pointers_present().is_some());
    }

    /// A non-leaf must carry a frame record whenever a leaf does. This holds
    /// under either `force-frame-pointers` setting and gives the probe a
    /// cross-check that needs no knowledge of the build flags.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn leaf_coverage_implies_non_leaf_coverage() {
        #[inline(never)]
        extern "C" fn probe_non_leaf(x: u64) -> u64 {
            // The call makes this a non-leaf under any setting.
            std::hint::black_box(x).wrapping_add(1)
        }
        let leaf = leaf_frame_pointers_present().expect("supported arch");
        // SAFETY: reading our own executable text, as in the production path.
        let non_leaf =
            unsafe { decode_prologue(probe_non_leaf as *const ()) }.expect("supported arch");
        if leaf {
            assert!(
                non_leaf,
                "leaf has a frame record but non-leaf does not; probe is misreading codegen"
            );
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn recognises_frame_pointer_establishing_instructions() {
        // mov x29, sp  == add x29, sp, #0
        assert!(establishes_frame_pointer_aarch64(0x9100_03FD));
        // add x29, sp, #16
        assert!(establishes_frame_pointer_aarch64(0x9100_43FD));
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn rejects_unrelated_instructions() {
        // add x0, sp, #0 - wrong Rd
        assert!(!establishes_frame_pointer_aarch64(0x9100_03E0));
        // add x29, x0, #0 - wrong Rn
        assert!(!establishes_frame_pointer_aarch64(0x9100_001D));
        // stp x29, x30, [sp, #-16]! - saves the pair but does not set x29
        assert!(!establishes_frame_pointer_aarch64(0xA9BF_7BFD));
        // ret
        assert!(!establishes_frame_pointer_aarch64(RET));
        // sub sp, sp, #16
        assert!(!establishes_frame_pointer_aarch64(0xD100_43FF));
    }
}
