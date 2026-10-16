//! `/proc/kallsyms` snapshot: `kptr_restrict` masks kernel addresses for non-root, and the process
//! may still be privileged when the recorder is built but not when the first segment is symbolized.
//! The snapshot is exactly as private as the process: `/proc/<pid>/fd/<n>` is readable by another
//! process only if this one is dumpable (`PR_SET_DUMPABLE`/`fs.suid_dumpable=1`) or the reader has
//! `CAP_SYS_PTRACE`.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

const KALLSYMS: &str = "/proc/kallsyms";

/// In-memory copy of `/proc/kallsyms`, readable through [`path`](Self::path).
pub(crate) struct KallsymsSnapshot {
    /// Keeps the memfd open: `path` is only valid while this fd is.
    _file: File,
    path: PathBuf,
}

impl KallsymsSnapshot {
    /// Copy `/proc/kallsyms` into a memfd.
    pub(crate) fn capture() -> io::Result<Self> {
        Self::from_bytes(&std::fs::read(KALLSYMS)?)
    }

    /// Build a snapshot from kallsyms-format bytes. Errors with `PermissionDenied` when every
    /// address is masked (all-zero), so callers fall back instead of symbolizing against zeros.
    pub(crate) fn from_bytes(contents: &[u8]) -> io::Result<Self> {
        if !has_unmasked_address(contents) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "/proc/kallsyms addresses are masked (kernel.kptr_restrict)",
            ));
        }
        // memfd, not a temp file: unmasked kernel addresses never touch disk; blazesym needs a
        // path, hence `/proc/self/fd/N`, which resolves through this process's fd table.
        // Raw syscall: glibc < 2.27 has no `memfd_create` wrapper.
        // SAFETY: the name is a valid NUL-terminated C string; the flags are a valid constant.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"dial9-kallsyms".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by memfd_create and is owned by nothing else.
        let mut file = unsafe { File::from_raw_fd(fd as libc::c_int) };
        file.write_all(contents)?;
        // Readers that reach the fd through procfs could otherwise rewrite the table (memfd inodes
        // are 0777); seal it so symbolization can only ever see what was captured.
        // SAFETY: plain fcntl on an fd we own.
        if unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        // Fail now (so callers fall back) if `/proc` cannot resolve the path.
        std::fs::metadata(&path)?;
        Ok(Self { _file: file, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for KallsymsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KallsymsSnapshot")
            .field("path", &self.path)
            .finish()
    }
}

/// Whether any line's leading address field is non-zero (i.e. not masked by `kptr_restrict`).
fn has_unmasked_address(kallsyms: &[u8]) -> bool {
    kallsyms.split(|&b| b == b'\n').any(|line| {
        line.split(|b| b.is_ascii_whitespace())
            .next()
            .is_some_and(|addr| addr.iter().any(|&b| b != b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_unmasked_address_detects_real_and_masked_addresses() {
        assert!(has_unmasked_address(b"ffffffff81000000 T _text\n"));
        assert!(!has_unmasked_address(
            b"0000000000000000 T _text\n0000000000000000 t foo\n"
        ));
        assert!(!has_unmasked_address(b""));
    }

    #[test]
    fn snapshot_is_sealed() {
        let snapshot = KallsymsSnapshot::from_bytes(b"ffffffff81000000 T _text\n").unwrap();

        // F_SEAL_WRITE makes write() fail with EPERM; opening for write may already fail.
        let write = std::fs::OpenOptions::new()
            .write(true)
            .open(snapshot.path())
            .and_then(|mut f| f.write_all(b"0000000000000000 T _text\n"));
        let err = write.expect_err("sealed snapshot must not be writable");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        let all = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        // SAFETY: plain fcntl on an fd the snapshot owns.
        let seals = unsafe { libc::fcntl(snapshot._file.as_raw_fd(), libc::F_GET_SEALS) };
        assert!(seals >= 0, "{}", io::Error::last_os_error());
        assert_eq!(seals & all, all, "seals = {seals:#x}");
        assert_eq!(
            std::fs::read(snapshot.path()).unwrap(),
            b"ffffffff81000000 T _text\n"
        );
    }

    #[test]
    fn capture_copies_kallsyms_or_reports_masked() {
        let live = std::fs::read(KALLSYMS).expect("read /proc/kallsyms");
        match KallsymsSnapshot::capture() {
            // Not byte-for-byte: BPF and module symbols can change between the two reads.
            Ok(snapshot) => {
                let copied = std::fs::read(snapshot.path()).expect("read snapshot");
                assert!(has_unmasked_address(&copied));
                let first_line = |b: &[u8]| b.split(|&c| c == b'\n').next().map(<[u8]>::to_vec);
                assert_eq!(first_line(&copied), first_line(&live));
                // (address, type) of the `_text` line.
                let text_entry = |b: &[u8]| {
                    String::from_utf8_lossy(b).lines().find_map(|l| {
                        let mut f = l.split_whitespace();
                        let (addr, ty, name) = (f.next()?, f.next()?, f.next()?);
                        (name == "_text").then(|| (addr.to_owned(), ty.to_owned()))
                    })
                };
                let text = text_entry(&copied);
                assert!(text.is_some(), "snapshot has no _text line");
                assert_eq!(text, text_entry(&live));
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
                assert!(!has_unmasked_address(&live));
            }
        }
    }
}
