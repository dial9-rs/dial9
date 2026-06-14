//! Per-process namespace isolation for shared trace directories.
//!
//! Each process writes to `{trace_dir}/{boot_id}/` so background workers
//! never cross-process. Liveness is tracked via `flock(LOCK_EX)` on
//! `{boot_id}/.lock`; dead peers are GC'd at startup.

use std::io;
use std::path::{Path, PathBuf};

use crate::primitives::fs;

// Startup-only operations that don't need shuttle fault-injection.
use std::fs as stdfs;

use crate::background_task::sealed::parse_segment_artifact;

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

/// Acquire an exclusive advisory lock on `{namespace_dir}/.lock`.
/// Returns the file handle — lock is held until the handle is dropped.
/// Kernel releases automatically on process death (including SIGKILL).
#[cfg(unix)]
pub(crate) fn acquire_namespace_lock(namespace_dir: &Path) -> io::Result<stdfs::File> {
    use std::os::unix::io::AsRawFd;

    fs::create_dir_all(namespace_dir)?;
    let lock_path = namespace_dir.join(".lock");
    let file = stdfs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;

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

#[cfg(not(unix))]
pub(crate) fn acquire_namespace_lock(namespace_dir: &Path) -> io::Result<stdfs::File> {
    fs::create_dir_all(namespace_dir)?;
    let lock_path = namespace_dir.join(".lock");
    stdfs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
}

#[cfg(unix)]
fn is_lock_held(namespace_dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;

    let lock_path = namespace_dir.join(".lock");
    let file = match stdfs::OpenOptions::new().read(true).open(&lock_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // Acquired — owner is dead. Release immediately.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

#[cfg(not(unix))]
fn is_lock_held(_namespace_dir: &Path) -> bool {
    // No flock — conservatively assume alive so GC never runs.
    true
}

fn is_safe_to_delete(entry: &stdfs::DirEntry, stem: &str) -> bool {
    let Ok(meta) = entry.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return false;
    };
    name == ".lock" || parse_segment_artifact(name, stem).is_some()
}

/// Infer the trace stem from any recognized file in the directory.
fn infer_stem(entries: &[stdfs::DirEntry]) -> Option<String> {
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str()?;
        if let Some(dot_pos) = name.find('.') {
            let candidate = &name[..dot_pos];
            if !candidate.is_empty() && parse_segment_artifact(name, candidate).is_some() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Fails closed: unrecognized files cause the directory to be skipped.
/// Never recursive.
pub(crate) fn gc_dead_namespaces(parent_dir: &Path, own_boot_id: &str) {
    let Ok(entries) = fs::read_dir(parent_dir) else {
        return;
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
        let _ = try_remove_namespace(&path);
    }
}

fn try_remove_namespace(dir: &Path) -> io::Result<()> {
    let entries: Vec<_> = stdfs::read_dir(dir)?.filter_map(|e| e.ok()).collect();

    let stem = infer_stem(&entries).unwrap_or_else(|| "trace".to_string());

    if entries.iter().any(|e| !is_safe_to_delete(e, &stem)) {
        return Ok(());
    }

    for entry in &entries {
        let _ = fs::remove_file(&entry.path());
    }
    let _ = stdfs::remove_dir(dir);
    Ok(())
}

pub(crate) fn setup_namespace(base_path: &Path) -> io::Result<(String, PathBuf, stdfs::File)> {
    let parent = base_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = base_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("trace.bin"));

    let boot_id = generate_boot_id();
    let ns_dir = parent.join(&boot_id);
    let lock_file = acquire_namespace_lock(&ns_dir)?;

    let rewritten_path = ns_dir.join(filename);
    gc_dead_namespaces(parent, &boot_id);

    Ok((boot_id, rewritten_path, lock_file))
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

    fn make_entry(dir: &Path, name: &str) -> stdfs::DirEntry {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        stdfs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name() == std::ffi::OsStr::new(name))
            .unwrap()
    }

    #[test]
    fn is_safe_to_delete_accepts_known() {
        let dir = TempDir::new().unwrap();
        assert!(is_safe_to_delete(&make_entry(dir.path(), ".lock"), "trace"));
        assert!(is_safe_to_delete(
            &make_entry(dir.path(), "trace.0.bin"),
            "trace"
        ));
        assert!(is_safe_to_delete(
            &make_entry(dir.path(), "trace.0.bin.active"),
            "trace"
        ));
        assert!(is_safe_to_delete(
            &make_entry(dir.path(), "trace.0.bin.gz"),
            "trace"
        ));
        assert!(is_safe_to_delete(
            &make_entry(dir.path(), "my-app.42.bin"),
            "my-app"
        ));
    }

    #[test]
    fn is_safe_to_delete_rejects_unknown() {
        let dir = TempDir::new().unwrap();
        assert!(!is_safe_to_delete(
            &make_entry(dir.path(), "README.md"),
            "trace"
        ));
        assert!(!is_safe_to_delete(
            &make_entry(dir.path(), "data.json"),
            "trace"
        ));
        assert!(!is_safe_to_delete(
            &make_entry(dir.path(), ".hidden"),
            "trace"
        ));
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

        let (boot_id, rewritten, _lock) = setup_namespace(&base).unwrap();

        assert!(is_valid_boot_id(&boot_id));
        assert_eq!(rewritten, dir.path().join(&boot_id).join("trace.bin"));
        assert!(dir.path().join(&boot_id).exists());
        assert!(dir.path().join(&boot_id).join(".lock").exists());
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
