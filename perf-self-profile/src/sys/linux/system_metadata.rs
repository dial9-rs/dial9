use std::ffi::CStr;
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::sync::OnceLock;

pub(crate) fn system_metadata() -> Vec<(String, String)> {
    static SYSTEM_METADATA: OnceLock<Vec<(String, String)>> = OnceLock::new();

    SYSTEM_METADATA.get_or_init(collect_system_metadata).clone()
}

fn collect_system_metadata() -> Vec<(String, String)> {
    let mut out = Vec::new();

    match uname_metadata() {
        Ok(entries) => out.extend(entries),
        Err(e) => tracing::warn!("failed to read uname metadata: {e}"),
    }

    match std::fs::read_to_string("/proc/cpuinfo").and_then(|value| {
        parse_cpu_model(&value).ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                "no supported CPU model field in /proc/cpuinfo",
            )
        })
    }) {
        Ok(model) => out.push(("cpu.profile.cpu_model".to_string(), model)),
        Err(e) => tracing::warn!("failed to read CPU model from /proc/cpuinfo: {e}"),
    }

    match std::fs::read_to_string("/proc/meminfo")
        .and_then(|value| parse_total_memory_bytes(&value))
    {
        Ok(total_bytes) => out.push((
            "cpu.profile.memory_total_bytes".to_string(),
            total_bytes.to_string(),
        )),
        Err(e) => tracing::warn!("failed to read total memory from /proc/meminfo: {e}"),
    }

    out
}

fn uname_metadata() -> io::Result<[(String, String); 4]> {
    let mut utsname = MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `utsname.as_mut_ptr()` points to valid storage for uname
    // to initialize.
    let rc = unsafe { libc::uname(utsname.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: uname returned success and initialized `utsname`.
    let utsname = unsafe { utsname.assume_init() };

    Ok([
        (
            "cpu.profile.machine_hostname".to_string(),
            uname_field(&utsname.nodename)?,
        ),
        (
            "cpu.profile.machine_architecture".to_string(),
            uname_field(&utsname.machine)?,
        ),
        (
            "cpu.profile.kernel_release".to_string(),
            uname_field(&utsname.release)?,
        ),
        (
            "cpu.profile.kernel_version".to_string(),
            uname_field(&utsname.version)?,
        ),
    ])
}

fn uname_field(field: &[libc::c_char]) -> io::Result<String> {
    // SAFETY: fields populated by uname are null-terminated C strings.
    let value = unsafe { CStr::from_ptr(field.as_ptr()) };

    value
        .to_str()
        .map(str::to_owned)
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))
}

fn parse_cpu_model(contents: &str) -> Option<String> {
    ["model name", "Processor", "Hardware"]
        .into_iter()
        .find_map(|wanted| {
            contents.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                let value = value.trim();
                (key.trim() == wanted && !value.is_empty()).then(|| value.to_string())
            })
        })
}

fn parse_total_memory_bytes(contents: &str) -> io::Result<u64> {
    let value = contents
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "MemTotal").then_some(value.trim())
        })
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                "MemTotal missing from /proc/meminfo",
            )
        })?;

    let mut parts = value.split_whitespace();
    let kibibytes = parts
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "MemTotal has no value"))?
        .parse::<u64>()
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    if parts.next() != Some("kB") || parts.next().is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "MemTotal must contain a value followed by kB",
        ));
    }

    kibibytes
        .checked_mul(1024)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "MemTotal overflows bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_model_uses_common_architecture_specific_fields() {
        let cpuinfo = r#"
Processor   : ARMv8 Processor rev 1 (v8l)
Hardware    : Generic DT based system
model name  : Neoverse-N1
"#;
        assert_eq!(parse_cpu_model(cpuinfo).as_deref(), Some("Neoverse-N1"));

        let cpuinfo = "processor : 0\nHardware : BCM2835\n";
        assert_eq!(parse_cpu_model(cpuinfo).as_deref(), Some("BCM2835"));
    }

    #[test]
    fn uname_metadata_contains_hostname_and_architecture() {
        let metadata = uname_metadata().unwrap();

        for key in [
            "cpu.profile.machine_hostname",
            "cpu.profile.machine_architecture",
        ] {
            let value = metadata
                .iter()
                .find_map(|(candidate, value)| (candidate == key).then_some(value))
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(!value.is_empty(), "{key} must not be empty");
        }
    }

    #[test]
    fn mem_total_kibibytes_are_converted_to_bytes() {
        let meminfo = "MemFree: 1024 kB\nMemTotal: 16343428 kB\n";
        assert_eq!(parse_total_memory_bytes(meminfo).unwrap(), 16_735_670_272);
    }

    #[test]
    fn malformed_mem_total_is_rejected() {
        let error = parse_total_memory_bytes("MemTotal: unlimited kB\n").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}
