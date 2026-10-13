//! A kallsyms snapshot taken as root still symbolizes kernel frames after the
//! process drops to an unprivileged user.
//!
//! This test changes the process's credentials irrevocably, so it must stay the
//! only test in this file: plain `cargo test` runs all of a binary's tests in one
//! process.
//!
//! Build as your normal user, then run only the built binary as root, so no
//! root-owned files land in `target/`:
//!
//! ```text
//! cargo nextest run -p dial9-perf-self-profile --features cpu-profiling,test-util \
//!     --run-ignored only -E 'binary(kallsyms_drop_root)' --no-run
//! sudo target/debug/deps/kallsyms_drop_root-<hash> --ignored --exact \
//!     kallsyms_snapshot_survives_dropping_root --nocapture
//! ```
//!
//! Find the binary with `cargo nextest list --message-format json` or
//! `ls target/debug/deps/kallsyms_drop_root-*`.
#![cfg(all(target_os = "linux", feature = "test-util"))]

use dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer;
use dial9_trace_format::{
    decoder::Decoder,
    encoder::Encoder,
    schema::FieldDef,
    types::{FieldType, FieldValue},
};

fn schedule_addr(kallsyms: &str) -> u64 {
    let line = kallsyms
        .lines()
        .find(|l| l.split_whitespace().nth(2) == Some("schedule"))
        .expect("schedule not found in /proc/kallsyms");
    u64::from_str_radix(line.split_whitespace().next().unwrap(), 16).unwrap()
}

fn symbol_names(symbolizer: &OfflineSymbolizer, addr: u64) -> Vec<String> {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
        .unwrap();
    enc.write_event(&schema, 0, &[FieldValue::StackFrames(vec![addr].into())])
        .unwrap();
    let buf = enc.finish();

    let output = symbolizer.symbolize(&buf, &[]).unwrap();
    let mut combined = buf;
    combined.extend_from_slice(&output);
    let mut dec = Decoder::new(&combined).unwrap();
    dec.decode_all();
    dec.string_pool()
        .iter()
        .map(|(_, s)| s.to_owned())
        .collect()
}

fn nobody_ids() -> (libc::uid_t, libc::gid_t) {
    // SAFETY: valid NUL-terminated name; the returned record is read before any other passwd call.
    let pw = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
    if pw.is_null() {
        // Conventional overflow/nobody id; any non-root id works.
        (65534, 65534)
    } else {
        // SAFETY: non-null pointer returned by getpwnam.
        unsafe { ((*pw).pw_uid, (*pw).pw_gid) }
    }
}

fn drop_root() {
    let (uid, gid) = nobody_ids();
    // SAFETY: plain credential syscalls; order matters (groups and gid before uid).
    unsafe {
        assert_eq!(libc::setgroups(0, std::ptr::null()), 0, "setgroups failed");
        assert_eq!(libc::setgid(gid), 0, "setgid failed");
        assert_eq!(libc::setuid(uid), 0, "setuid failed");
    }
    // SAFETY: no preconditions.
    assert_ne!(unsafe { libc::geteuid() }, 0, "still root after setuid");
    // SAFETY: plain credential syscall.
    assert_ne!(
        unsafe { libc::setuid(0) },
        0,
        "regained root: the drop must be irrevocable"
    );
}

#[test]
#[ignore = "needs root: build with `cargo nextest run -p dial9-perf-self-profile --features cpu-profiling,test-util --run-ignored only -E 'binary(kallsyms_drop_root)' --no-run`, then `sudo target/debug/deps/kallsyms_drop_root-<hash> --ignored --exact kallsyms_snapshot_survives_dropping_root`"]
fn kallsyms_snapshot_survives_dropping_root() {
    // SAFETY: no preconditions.
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "this test must run as root (see the module docs for the recipe)"
    );
    let kptr_restrict: u32 = std::fs::read_to_string("/proc/sys/kernel/kptr_restrict")
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    if kptr_restrict >= 2 {
        eprintln!("skipping: kptr_restrict=2 masks even root");
        return;
    }

    let snapshot = OfflineSymbolizer::try_with_kallsyms_snapshot().expect("snapshot while root");
    let self_path = snapshot
        .kallsyms_snapshot_path()
        .expect("snapshot symbolizer has a snapshot path");
    let procfs_path = self_path
        .to_string_lossy()
        .replace("/proc/self/", &format!("/proc/{}/", std::process::id()));
    assert_ne!(
        procfs_path,
        self_path.to_string_lossy(),
        "pid rewrite failed"
    );
    let addr = schedule_addr(&std::fs::read_to_string("/proc/kallsyms").unwrap());
    assert_ne!(addr, 0, "schedule address is masked even for root");

    drop_root();

    if kptr_restrict > 0 {
        let masked = schedule_addr(&std::fs::read_to_string("/proc/kallsyms").unwrap());
        assert_eq!(
            masked, 0,
            "/proc/kallsyms must be masked after dropping root"
        );
    }

    // The snapshot's only handle is this process's fd table. After the drop the process is
    // non-dumpable, so a same-uid process must not be able to open it through procfs.
    // SAFETY: plain prctl query.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE) },
        0,
        "expected non-dumpable after setuid"
    );
    // MFD_CLOEXEC keeps the fd itself out of the child.
    let out = std::process::Command::new("cat")
        .arg(&procfs_path)
        .output()
        .expect("spawn cat");
    assert!(
        !out.status.success(),
        "same-uid child read the snapshot: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Permission denied"),
        "unexpected cat failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The symbolizer thread spawns lazily, so both open their kallsyms source as non-root.
    let resolved = symbol_names(&snapshot, addr);
    assert!(
        resolved.iter().any(|s| s == "schedule"),
        "snapshot must resolve `schedule` after dropping root, got: {resolved:?}"
    );

    let live = symbol_names(&OfflineSymbolizer::new(), addr);
    if kptr_restrict > 0 {
        assert!(
            !live.iter().any(|s| s == "schedule"),
            "live /proc/kallsyms must not resolve `schedule` as non-root, got: {live:?}"
        );
    }
}
