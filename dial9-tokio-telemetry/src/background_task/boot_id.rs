//! Per-process namespace isolation for shared trace directories.
//!
//! Each process writes to `{trace_dir}/{boot_id}/` so background workers
//! never cross-process. Liveness is tracked via `flock(LOCK_EX)` on
//! `{boot_id}/.lock`; dead peers are GC'd at startup.

use std::io;
use std::path::{Path, PathBuf};

use crate::primitives::fs;

/// Generate a boot identifier of the form `{4-alpha}-{pid}` (e.g. `qmxz-481`).
///
/// The 4 letters are derived from the current system-time nanoseconds and the
/// pid makes it unique among live processes. Also used by the S3 uploader as
/// `S3Config`'s default `boot_id`, so both the on-disk namespace and S3 keys
/// share one identity format.
pub(crate) fn generate_boot_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut v = nanos as u64;
    let mut s = String::with_capacity(10);
    for _ in 0..4 {
        s.push((b'a' + (v % 26) as u8) as char);
        v /= 26;
    }
    s.push_str(&format!("-{}", std::process::id()));
    s
}

/// Matches `^[a-z]{4}-[0-9]+$`.
pub(crate) fn is_valid_boot_id(name: &str) -> bool {
    let Some((alpha, pid)) = name.split_once('-') else {
        return false;
    };
    alpha.len() == 4
        && alpha.bytes().all(|b| b.is_ascii_lowercase())
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
}

/// Advisory file locking via `flock(2)`. Confined to one place so the only
/// `unsafe` in this module lives behind a small, documented API.
///
/// On non-unix targets there is no `flock`, so [`is_held`] conservatively
/// reports "held" and the startup GC never reclaims a peer — isolation still
/// works, only cross-process cleanup is skipped.
#[cfg(unix)]
mod flock {
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    // The lock fd is a real OS handle; shuttle can't model `flock`, so the
    // lockfile open/lock path deliberately uses `std::fs` rather than the
    // crate's shuttle-aware `primitives::fs`.
    use std::fs::{File, OpenOptions};

    /// Open (creating if needed) and exclusively `flock` `path`, non-blocking.
    /// The lock is held until the returned [`File`] is dropped; the kernel
    /// releases it automatically on process death (including SIGKILL).
    /// Returns `WouldBlock` if another open file description holds the lock.
    pub(super) fn acquire(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: `as_raw_fd` returns a valid fd owned by `file`, which
        // outlives this call. `flock` only reads the fd and the constant
        // flags; no memory is involved.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("namespace lock held by another process: {err}"),
            ));
        }
        Ok(file)
    }

    /// Whether `path`'s lock is currently held by some live process. Probes by
    /// trying to acquire it from a fresh fd: success means the owner is gone.
    pub(super) fn is_held(path: &Path) -> bool {
        let Ok(file) = OpenOptions::new().read(true).open(path) else {
            return false;
        };
        // SAFETY: same as `acquire` — `file` owns the fd for the duration.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            // We got it, so no one else holds it: release our probe lock.
            // SAFETY: same fd, still owned by `file`.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            false
        } else {
            true
        }
    }
}

/// Acquire the namespace lock at `{namespace_dir}/.lock`, creating the
/// directory if needed. The returned handle must be held for the process
/// lifetime; dropping it releases the lock.
#[cfg(unix)]
pub(crate) fn acquire_namespace_lock(namespace_dir: &Path) -> io::Result<std::fs::File> {
    fs::create_dir_all(namespace_dir)?;
    flock::acquire(&namespace_dir.join(".lock"))
}

#[cfg(not(unix))]
pub(crate) fn acquire_namespace_lock(namespace_dir: &Path) -> io::Result<std::fs::File> {
    fs::create_dir_all(namespace_dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(namespace_dir.join(".lock"))
}

/// Whether `namespace_dir`'s owner is still alive. On non-unix there is no
/// `flock`, so this conservatively returns `true` and GC never runs.
fn is_lock_held(namespace_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        flock::is_held(&namespace_dir.join(".lock"))
    }
    #[cfg(not(unix))]
    {
        let _ = namespace_dir;
        true
    }
}

/// Whether a file is one this crate creates and may therefore delete: the
/// `.lock` or any `{stem}.{index}.bin[.gz|.active]` segment artifact, for any
/// stem.
fn is_recognized_artifact(name: &str) -> bool {
    if name == ".lock" {
        return true;
    }
    let Some((head, tail)) = name.split_once(".bin") else {
        return false;
    };
    if !tail.is_empty() && tail != ".gz" && tail != ".active" {
        return false;
    }
    // `head` is `{stem}.{index}` — require a trailing `.{digits}` index.
    matches!(head.rsplit_once('.'), Some((_, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()))
}

/// Reclaim dead peers' namespace directories under `parent_dir`. A directory
/// is eligible only if its name is a boot_id, it isn't ours, its lock is free
/// (owner dead), and it contains nothing but recognized artifacts. Fails
/// closed: an unrecognized file leaves the directory untouched. Never
/// recursive.
pub(crate) fn gc_dead_namespaces(parent_dir: &Path, own_boot_id: &str) {
    let entries = match fs::read_dir(parent_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(
                target: "dial9_worker",
                error = %e,
                dir = %parent_dir.display(),
                "namespace GC: failed to scan parent directory"
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == own_boot_id || !is_valid_boot_id(name) || is_lock_held(&path) {
            continue;
        }
        if let Err(e) = try_remove_namespace(&path) {
            tracing::debug!(
                target: "dial9_worker",
                error = %e,
                dir = %path.display(),
                "namespace GC: failed to reclaim dead peer"
            );
        }
    }
}

fn try_remove_namespace(dir: &Path) -> io::Result<()> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        match name.to_str() {
            Some(name) if is_recognized_artifact(name) => paths.push(entry.path()),
            // Anything we don't recognize (or a non-UTF-8 name) means this
            // isn't safely ours to delete — fail closed, leave it alone.
            _ => return Ok(()),
        }
    }

    for path in paths {
        fs::remove_file(&path)?;
    }
    fs::remove_dir(dir)
}

/// Result of setting up a per-process namespace: the boot id, the rewritten
/// trace path inside `{parent}/{boot_id}/`, and the lock handle that must be
/// kept alive for the process lifetime.
pub(crate) struct Namespace {
    pub(crate) boot_id: String,
    pub(crate) trace_path: PathBuf,
    pub(crate) lock: std::fs::File,
}

/// Create `{parent}/{boot_id}/` under `base_path`'s directory, lock it, and
/// rewrite the trace path to live inside it. When `gc` is true, dead peers'
/// namespaces under the same parent are reclaimed first.
pub(crate) fn setup_namespace(base_path: &Path, gc: bool) -> io::Result<Namespace> {
    let parent = base_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = base_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("trace.bin"));

    let boot_id = generate_boot_id();
    let ns_dir = parent.join(&boot_id);
    let lock = acquire_namespace_lock(&ns_dir)?;

    if gc {
        gc_dead_namespaces(parent, &boot_id);
    }

    Ok(Namespace {
        boot_id,
        trace_path: ns_dir.join(filename),
        lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_boot_id_matches_pattern() {
        let id = generate_boot_id();
        assert!(is_valid_boot_id(&id), "boot_id {id:?} should match pattern");
        let (alpha, pid) = id.split_once('-').unwrap();
        assert_eq!(alpha.len(), 4);
        assert!(alpha.chars().all(|c| c.is_ascii_lowercase()));
        assert!(!pid.is_empty());
        assert!(pid.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn is_valid_boot_id_accepts_valid() {
        assert!(is_valid_boot_id("abcd-1234"));
        assert!(is_valid_boot_id("zzzz-1"));
        assert!(is_valid_boot_id("aaaa-99999"));
    }

    #[test]
    fn is_valid_boot_id_rejects_invalid() {
        assert!(!is_valid_boot_id("abc-1234"));
        assert!(!is_valid_boot_id("abcde-1234"));
        assert!(!is_valid_boot_id("ABCD-1234"));
        assert!(!is_valid_boot_id("abcd1234"));
        assert!(!is_valid_boot_id("abcd-"));
        assert!(!is_valid_boot_id("abcd-abc"));
        assert!(!is_valid_boot_id(""));
    }

    #[test]
    fn is_recognized_artifact_accepts_known() {
        assert!(is_recognized_artifact(".lock"));
        assert!(is_recognized_artifact("trace.0.bin"));
        assert!(is_recognized_artifact("trace.0.bin.active"));
        assert!(is_recognized_artifact("trace.0.bin.gz"));
        assert!(is_recognized_artifact("my-app.42.bin"));
        assert!(is_recognized_artifact("some.other.stem.7.bin.gz"));
    }

    #[test]
    fn is_recognized_artifact_rejects_unknown() {
        assert!(!is_recognized_artifact("README.md"));
        assert!(!is_recognized_artifact("data.json"));
        assert!(!is_recognized_artifact(".hidden"));
        assert!(!is_recognized_artifact("trace.bin")); // no index
        assert!(!is_recognized_artifact("trace.x.bin")); // non-numeric index
        assert!(!is_recognized_artifact("trace.0.bin.tmp")); // unknown suffix
    }

    #[cfg(unix)]
    #[test]
    fn acquire_and_detect_lock() {
        let dir = TempDir::new().unwrap();
        let ns_dir = dir.path().join("abcd-1234");

        assert!(!is_lock_held(&ns_dir));

        let _lock = acquire_namespace_lock(&ns_dir).unwrap();
        assert!(is_lock_held(&ns_dir));

        drop(_lock);
        assert!(!is_lock_held(&ns_dir));
    }

    #[cfg(unix)]
    #[test]
    fn setup_namespace_creates_subdir_and_rewrites_path() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace.bin");

        let ns = setup_namespace(&base, true).unwrap();

        assert!(is_valid_boot_id(&ns.boot_id));
        assert_eq!(
            ns.trace_path,
            dir.path().join(&ns.boot_id).join("trace.bin")
        );
        assert!(dir.path().join(&ns.boot_id).exists());
        assert!(dir.path().join(&ns.boot_id).join(".lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn setup_namespace_without_gc_keeps_dead_peer() {
        let dir = TempDir::new().unwrap();
        let dead_ns = dir.path().join("dead-9999");
        std::fs::create_dir(&dead_ns).unwrap();
        std::fs::write(dead_ns.join(".lock"), b"").unwrap();
        std::fs::write(dead_ns.join("trace.0.bin"), b"data").unwrap();

        let base = dir.path().join("trace.bin");
        let _ns = setup_namespace(&base, false).unwrap();

        // GC disabled, so the dead peer's directory survives.
        assert!(dead_ns.exists());
    }

    /// flock is associated with the open file description, so a *second*
    /// independent `open()` of the same lock file conflicts even from the same
    /// process. This is the contention a second live process would hit, which
    /// is what `is_lock_held` relies on to detect a live owner.
    #[cfg(unix)]
    #[test]
    fn second_acquire_conflicts_while_first_held() {
        let dir = TempDir::new().unwrap();
        let ns_dir = dir.path().join("abcd-1234");

        let first = acquire_namespace_lock(&ns_dir).unwrap();

        let second = acquire_namespace_lock(&ns_dir);
        assert!(
            matches!(&second, Err(e) if e.kind() == io::ErrorKind::WouldBlock),
            "second acquire of a held lock must fail with WouldBlock, got {second:?}"
        );

        // Releasing the first lets the next acquire succeed.
        drop(first);
        assert!(acquire_namespace_lock(&ns_dir).is_ok());
    }

    /// Two concurrent `setup_namespace` calls (two "live processes") must land
    /// in distinct directories, each holding its own lock, and neither GC's the
    /// other.
    #[cfg(unix)]
    #[test]
    fn two_live_owners_get_isolated_namespaces() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace.bin");

        let a = setup_namespace(&base, true).unwrap();
        let b = setup_namespace(&base, true).unwrap();

        // Distinct namespaces.
        assert_ne!(
            a.boot_id, b.boot_id,
            "two owners must get distinct boot_ids"
        );
        assert!(a.trace_path.starts_with(dir.path().join(&a.boot_id)));
        assert!(b.trace_path.starts_with(dir.path().join(&b.boot_id)));

        // Each owner still holds its lock — neither GC'd the other.
        assert!(dir.path().join(&a.boot_id).join(".lock").exists());
        assert!(dir.path().join(&b.boot_id).join(".lock").exists());
        // The live locks are observed as held from a fresh fd.
        assert!(is_lock_held(&dir.path().join(&a.boot_id)));
        assert!(is_lock_held(&dir.path().join(&b.boot_id)));
    }

    /// A peer whose lock is still held (alive) must NOT be reclaimed by GC,
    /// even though its directory contains only recognized files.
    #[cfg(unix)]
    #[test]
    fn gc_preserves_live_peer_holding_lock() {
        let dir = TempDir::new().unwrap();
        let peer = dir.path().join("abcd-1");
        let _peer_lock = acquire_namespace_lock(&peer).unwrap();
        std::fs::write(peer.join("trace.0.bin"), b"live data").unwrap();

        gc_dead_namespaces(dir.path(), "zzzz-2");

        assert!(peer.exists(), "live peer must survive GC");
        assert!(peer.join("trace.0.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_removes_dead_namespace() {
        let dir = TempDir::new().unwrap();
        let dead_ns = dir.path().join("dead-9999");
        std::fs::create_dir(&dead_ns).unwrap();
        std::fs::write(dead_ns.join(".lock"), b"").unwrap();
        std::fs::write(dead_ns.join("trace.0.bin"), b"data").unwrap();
        std::fs::write(dead_ns.join("trace.0.bin.gz"), b"data").unwrap();

        gc_dead_namespaces(dir.path(), "live-1234");

        assert!(!dead_ns.exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_skips_namespace_with_unrecognized_file() {
        let dir = TempDir::new().unwrap();
        let dead_ns = dir.path().join("dead-9999");
        std::fs::create_dir(&dead_ns).unwrap();
        std::fs::write(dead_ns.join(".lock"), b"").unwrap();
        std::fs::write(dead_ns.join("important.txt"), b"keep me").unwrap();

        gc_dead_namespaces(dir.path(), "live-1234");

        assert!(dead_ns.exists());
        assert!(dead_ns.join("important.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_skips_live_namespace() {
        let dir = TempDir::new().unwrap();
        let live_ns = dir.path().join("live-1234");

        let _lock = acquire_namespace_lock(&live_ns).unwrap();
        std::fs::write(live_ns.join("trace.0.bin"), b"data").unwrap();

        gc_dead_namespaces(dir.path(), "other-5678");

        assert!(live_ns.exists());
        assert!(live_ns.join("trace.0.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_skips_non_boot_id_directories() {
        let dir = TempDir::new().unwrap();
        let other = dir.path().join("not-a-boot-id");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("data.bin"), b"x").unwrap();

        gc_dead_namespaces(dir.path(), "live-1234");

        assert!(other.exists());
    }
}
