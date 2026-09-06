//! NVIDIA CUDA GPU metrics sampled through NVML.
//!
//! [NVIDIA Management Library (NVML)](https://developer.nvidia.com/management-library-nvml)
//! is loaded dynamically, so enabling the Cargo feature does not require the CUDA
//! toolkit or NVIDIA driver at build time. [`CudaGpuSource::start`] returns an
//! error when NVML is absent, allowing callers to disable the source cleanly on
//! hosts without NVIDIA GPUs.

use dial9_core::clock::clock_monotonic_ns;
use dial9_core::rate_limited;
use dial9_core::source::{FlushContext, Source};
use nvml_wrapper::enum_wrappers::device::PcieUtilCounter;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::{Device, Nvml};
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// Failure to initialize CUDA GPU profiling.
#[derive(Debug)]
#[non_exhaustive]
pub enum CudaGpuStartError {
    /// NVML could not be loaded, initialized, or queried.
    Nvml(NvmlError),
    /// NVML initialized successfully but reported no NVIDIA GPUs.
    NoDevices,
}

impl fmt::Display for CudaGpuStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nvml(error) => write!(f, "NVML initialization failed: {error}"),
            Self::NoDevices => f.write_str("NVML reported no NVIDIA GPU devices"),
        }
    }
}

impl Error for CudaGpuStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Nvml(error) => Some(error),
            Self::NoDevices => None,
        }
    }
}

impl From<NvmlError> for CudaGpuStartError {
    fn from(error: NvmlError) -> Self {
        Self::Nvml(error)
    }
}

/// A point-in-time snapshot of one NVIDIA GPU.
#[derive(Debug, dial9_trace_format::TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct CudaGpuEvent {
    /// Monotonic timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// NVML device index at source startup.
    pub device_index: u32,
    /// Stable NVIDIA GPU UUID.
    pub device_uuid: String,
    /// Human-readable NVIDIA product name.
    pub device_name: String,
    /// Time during which at least one kernel executed, as a percentage of the
    /// device's most recent NVML sample period.
    #[traceevent(kind = "gauge")]
    pub compute_utilization_percent: u32,
    /// Time during which device memory was read or written, as a percentage of
    /// the device's most recent NVML sample period.
    #[traceevent(kind = "gauge")]
    pub memory_io_utilization_percent: u32,
    /// Allocated device framebuffer memory across all processes.
    #[traceevent(unit = "bytes", kind = "gauge")]
    pub memory_used_bytes: u64,
    /// Total installed device framebuffer memory.
    #[traceevent(unit = "bytes", kind = "gauge")]
    pub memory_total_bytes: u64,
    /// Aggregate GPU-to-host PCIe throughput in KiB/s, when supported.
    #[traceevent(kind = "gauge")]
    pub pcie_to_host_kib_per_second: Option<u32>,
    /// Aggregate host-to-GPU PCIe throughput in KiB/s, when supported.
    #[traceevent(kind = "gauge")]
    pub pcie_from_host_kib_per_second: Option<u32>,
}

/// Configuration for NVIDIA CUDA GPU sampling.
#[derive(Debug, Clone, bon::Builder)]
pub struct CudaGpuConfig {
    /// Minimum time between samples.
    #[builder(default = DEFAULT_SAMPLE_INTERVAL)]
    sample_interval: Duration,
    /// Whether to sample aggregate PCIe transfer throughput.
    #[builder(default = true)]
    sample_pcie_throughput: bool,
    /// NVML device index to sample. By default, the device at index 0 is sampled.
    device_index: Option<u32>,
}

impl Default for CudaGpuConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl CudaGpuConfig {
    /// Minimum time between samples.
    pub fn sample_interval(&self) -> Duration {
        self.sample_interval
    }

    /// Whether aggregate PCIe transfer throughput is sampled.
    pub fn sample_pcie_throughput(&self) -> bool {
        self.sample_pcie_throughput
    }

    /// NVML device index to sample, or `None` to sample just the first device.
    pub fn device_index(&self) -> Option<u32> {
        self.device_index
    }
}

#[derive(Debug)]
struct DeviceIdentity {
    index: u32,
    uuid: String,
    name: String,
}

/// Flush-thread source that samples the configured NVIDIA GPUs through NVML.
#[derive(Debug)]
pub struct CudaGpuSource {
    nvml: Nvml,
    config: CudaGpuConfig,
    device: DeviceIdentity,
    last_sample: Option<Instant>,
}

impl CudaGpuSource {
    /// Initialize NVML and discover the configured NVIDIA GPUs.
    ///
    /// This returns an error if the NVML shared library or driver is absent. No
    /// CUDA context is created, and the CUDA toolkit is not required.
    pub fn start(config: CudaGpuConfig) -> Result<Self, CudaGpuStartError> {
        let nvml = Nvml::init()?;
        let device_count = nvml.device_count()?;
        if device_count == 0 {
            return Err(CudaGpuStartError::NoDevices);
        }
        let index = config.device_index.unwrap_or_default();

        let device = nvml.device_by_index(index)?;
        let device = DeviceIdentity {
            index,
            uuid: device.uuid()?,
            name: device.name()?,
        };

        Ok(Self {
            nvml,
            config,
            device,
            last_sample: None,
        })
    }
}

impl Source for CudaGpuSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        let now = Instant::now();
        if let Some(last_sample) = self.last_sample
            && now.duration_since(last_sample) < self.config.sample_interval
        {
            return;
        }
        self.last_sample = Some(now);

        let timestamp_ns = clock_monotonic_ns();
        let result = (|| {
            let device = self.nvml.device_by_index(self.device.index)?;
            let utilization = device.utilization_rates()?;
            let memory = device.memory_info()?;
            let (to_host, from_host) = if self.config.sample_pcie_throughput {
                (
                    optional_pcie_throughput(&device, PcieUtilCounter::Send)?,
                    optional_pcie_throughput(&device, PcieUtilCounter::Receive)?,
                )
            } else {
                (None, None)
            };

            Ok::<_, NvmlError>(CudaGpuEvent {
                timestamp_ns,
                device_index: self.device.index,
                device_uuid: self.device.uuid.clone(),
                device_name: self.device.name.clone(),
                compute_utilization_percent: utilization.gpu,
                memory_io_utilization_percent: utilization.memory,
                memory_used_bytes: memory.used,
                memory_total_bytes: memory.total,
                pcie_to_host_kib_per_second: to_host,
                pcie_from_host_kib_per_second: from_host,
            })
        })();

        match result {
            Ok(event) => ctx.record_event(&event),
            Err(e) => rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    device_index = self.device.index,
                    device_uuid = self.device.uuid,
                    "failed to sample NVIDIA GPU via NVML: {e}"
                );
            }),
        }
    }

    fn name(&self) -> &'static str {
        "cuda_gpu"
    }
}

fn optional_pcie_throughput(
    device: &Device<'_>,
    counter: PcieUtilCounter,
) -> Result<Option<u32>, NvmlError> {
    match device.pcie_throughput(counter) {
        Ok(value) => Ok(Some(value)),
        Err(NvmlError::NotSupported) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::TraceEvent;

    #[test]
    fn default_configuration_samples_pcie_every_200ms() {
        let config = CudaGpuConfig::default();
        assert_eq!(config.sample_interval(), Duration::from_millis(200));
        assert!(config.sample_pcie_throughput());
        assert_eq!(config.device_index(), None);
    }

    #[test]
    fn configuration_accepts_a_single_device_index() {
        let config = CudaGpuConfig::builder().device_index(3).build();
        assert_eq!(config.device_index(), Some(3));
    }

    #[test]
    fn gpu_measurements_are_gauges_with_units() {
        let entry = CudaGpuEvent::schema_entry();
        let annotations = entry
            .annotations()
            .iter()
            .map(|annotation| {
                (
                    entry.fields()[annotation.field_index() as usize].name(),
                    annotation.key(),
                    annotation.value(),
                )
            })
            .collect::<Vec<_>>();

        assert!(annotations.contains(&("compute_utilization_percent", "kind", "gauge")));
        assert!(annotations.contains(&("memory_used_bytes", "unit", "bytes")));
        assert!(annotations.contains(&("pcie_to_host_kib_per_second", "kind", "gauge")));
    }
}
