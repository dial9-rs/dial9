//! Bionic aarch64 `ucontext`/`sigcontext` layout.
//!
//! The `libc` crate's `ucontext_t` for Android is missing the 120-byte
//! `__padding` field Bionic inserts between `uc_sigmask` (8 bytes) and
//! `uc_mcontext` to pad the sigmask area to 128 bytes. Reading
//! `(*libc::ucontext_t).uc_mcontext.pc` therefore reads from the wrong offset
//! and corrupts the walk. We use Bionic's documented layout instead.

#![cfg(all(target_os = "android", target_arch = "aarch64"))]
#![allow(nonstandard_style)]

use ::core::ffi::*;

/// Read the PC register from a ucontext on Android aarch64.
pub unsafe fn android_ucontext_pc(ucontext: *mut c_void) -> u64 {
    // The legacy hard-coded offset, kept as a sanity check on the struct layout.
    const {
        assert! {
            ::core::mem::offset_of!(struct_ucontext, uc_mcontext.pc) == 440
        };
    }
    unsafe { (*ucontext.cast::<struct_ucontext>()).uc_mcontext.pc }
}

/// Set the PC register in an Android aarch64 ucontext.
pub unsafe fn android_ucontext_set_pc(ucontext: *mut c_void, pc: u64) {
    unsafe { (*ucontext.cast::<struct_ucontext>()).uc_mcontext.pc = pc };
}

/// Set `x0`, the return-value register, in an Android aarch64 ucontext.
pub unsafe fn android_ucontext_set_result_reg(ucontext: *mut c_void, value: u64) {
    unsafe { (*ucontext.cast::<struct_ucontext>()).uc_mcontext.regs[0] = value };
}

/// Read `(pc, fp, sp)` from an Android aarch64 ucontext. `fp` is `x29` (the
/// AAPCS64 frame pointer register).
#[inline]
pub unsafe fn android_ucontext_pc_fp_sp(ucontext: *mut c_void) -> (u64, u64, u64) {
    unsafe {
        let mc = &(*ucontext.cast::<struct_ucontext>()).uc_mcontext;
        (mc.pc, mc.regs[29], mc.sp)
    }
}

/// See <https://android.googlesource.com/platform/bionic/+/731631f300090436d7f5df80d50b6275c8c60a93/libc/kernel/uapi/asm-arm64/asm/ucontext.h>
///
/// ```c
/// struct ucontext {
///         unsigned long uc_flags;
///     struct ucontext * uc_link;
///               stack_t uc_stack;
///              sigset_t uc_sigmask;
///                  __u8 __linux_unused[1024 / 8 - sizeof(sigset_t)];
///     struct sigcontext uc_mcontext;
/// };
/// ```
#[repr(C)]
pub struct struct_ucontext {
    pub uc_flags: c_ulong,
    pub uc_link: *mut struct_ucontext,
    pub uc_stack: stack_t,
    pub uc_sigmask: reserved_sigset_t,
    pub uc_mcontext: struct_sigcontext,
}

/// The proper way to encode this padding (as a matter of fact; C should have done the same…).
#[repr(C)]
pub union reserved_sigset_t {
    pub actual: sigset_t,
    pub reserved: [u8; 1024 / 8],
}

/// See <https://android.googlesource.com/platform/bionic/+/731631f300090436d7f5df80d50b6275c8c60a93/libc/kernel/uapi/asm-arm64/asm/ucontext.h>
///
/// ```c
/// typedef struct sigaltstack {
///           void  * ss_sp;
///               int ss_flags;
///   __kernel_size_t ss_size;
/// } stack_t;
/// ```
#[repr(C)]
pub struct stack_t {
    pub ss_sp: *mut c_void,
    pub ss_flags: c_int,
    pub ss_size: kernel_size_t,
}

/// See <https://android.googlesource.com/platform/bionic/+/731631f300090436d7f5df80d50b6275c8c60a93/libc/kernel/uapi/asm-generic/posix_types.h#47>
pub type kernel_size_t = c_ulong;

/// <https://android.googlesource.com/platform/bionic/+/731631f300090436d7f5df80d50b6275c8c60a93/libc/kernel/uapi/asm-generic/signal.h#58>
///
/// ```c
/// #define _NSIG 64
/// #define _NSIG_BPW __BITS_PER_LONG   // 64 on aarch64
/// #define _NSIG_WORDS (_NSIG / _NSIG_BPW)  // 1
///
/// typedef struct {
///     unsigned long sig[_NSIG_WORDS];
/// } sigset_t;
/// ```
#[derive(Clone, Copy)]
#[repr(C)]
pub struct sigset_t {
    pub sig: [c_ulong; 1],
}

/// <https://android.googlesource.com/platform/bionic/+/731631f300090436d7f5df80d50b6275c8c60a93/libc/kernel/uapi/asm-arm64/asm/sigcontext.h#11>
///
/// ```c
/// struct sigcontext {
///   __u64 fault_address;
///   __u64 regs[31];
///   __u64 sp;
///   __u64 pc;
///   __u64 pstate;
///    __u8 __reserved[4096] __attribute__((__aligned__(16)));
/// };
/// ```
#[repr(C)]
pub struct struct_sigcontext {
    pub fault_address: u64,
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    pub __reserved: Align16<[u8; 4096]>,
}

#[repr(C, align(16))]
pub struct Align16<T>(pub T);
