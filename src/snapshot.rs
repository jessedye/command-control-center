//! Data model shared by the local collector, SSH collectors, and the k8s fleet panel.

/// One disk/mount capacity row. Byte fields mirror statvfs: `free_bytes` includes the
/// root-reserve blocks, `avail_bytes` excludes them. `pct()` reproduces `df`'s Use%
/// exactly (used / (used + avail)), which is what operators mentally compare against.
#[derive(Debug, Clone, PartialEq)]
pub struct DiskRow {
    pub mount: String,
    pub fstype: String,
    /// Device node backing the mount (`/dev/nvme0n1p3`). sysinfo exposes it; remote frames
    /// carry none (""), which is also the correct dedup fallback key for exotic filesystems.
    pub device: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub avail_bytes: u64,
}

impl DiskRow {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    pub fn pct(&self) -> f64 {
        let used = self.used_bytes();
        // Saturating: round-2 finding 25 proved a hostile frame could push the sum past
        // u64 wrap-around (decode() now rejects free/avail > total, this is the last guard).
        let denom = used.saturating_add(self.avail_bytes);
        if denom == 0 {
            return 0.0;
        }
        100.0 * used as f64 / denom as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuRow {
    pub vendor: GpuVendor,
    pub name: String,
    pub vram_used_mib: f64,
    pub vram_total_mib: f64,
    pub util_pct: f64,
    pub temp_c: Option<f64>,
}

impl GpuRow {
    pub fn vram_pct(&self) -> f64 {
        if self.vram_total_mib > 0.0 {
            100.0 * self.vram_used_mib / self.vram_total_mib
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub user: String,
    pub cpu_pct: f32, // percent of ONE core, top(1) Irix style (can exceed 100 across cores)
    pub mem_pct: f32,
    pub rss_kb: u64,
    pub elapsed_s: u64,
    pub cmd: String,
}

/// Network interface rate over the last sample window.
#[derive(Debug, Clone, PartialEq)]
pub struct NetRow {
    pub iface: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub link_speed_mbps: Option<u32>,
}

/// Full telemetry frame for one station, produced every tick by a collector.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    /// Display label, e.g. `orion07`.
    pub name: String,
    pub ts_epoch: i64,
    pub os_name: String,
    pub kernel: String,
    pub uptime_s: u64,
    pub load: [f32; 3],
    pub core_count: usize,
    pub cpu_avg_pct: f32,
    pub per_core_pct: Vec<f32>,
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    pub disks: Vec<DiskRow>,
    pub net: Vec<NetRow>,
    pub gpus: Vec<GpuRow>,
    pub procs: Vec<ProcRow>,
}

impl Station {
    pub fn mem_used_kb(&self) -> u64 {
        self.mem_total_kb.saturating_sub(self.mem_avail_kb)
    }
    pub fn mem_pct(&self) -> f64 {
        pct_of(self.mem_used_kb(), self.mem_total_kb)
    }
    pub fn swap_used_kb(&self) -> u64 {
        self.swap_total_kb.saturating_sub(self.swap_free_kb)
    }
    pub fn swap_pct(&self) -> f64 {
        pct_of(self.swap_used_kb(), self.swap_total_kb)
    }
}

pub fn pct_of(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * used as f64 / total as f64
    }
}

/// Kubernetes node summary from `kubectl get nodes` + `kubectl top nodes`.
#[derive(Debug, Clone, PartialEq)]
pub struct KubeNode {
    pub name: String,
    pub status: String,
    pub roles: String,
    pub age: String,
    pub version: String,
    pub cpu_millis: Option<u64>,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub mem_pct: Option<f32>,
}

/// One container's live usage from `kubectl top pods --containers`. Kubernetes exposes
/// workloads as pods/containers, not OS processes — this is what kubectl can honestly see.
#[derive(Debug, Clone, PartialEq)]
pub struct KubePod {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub cpu_milli: u64,
    pub mem_bytes: u64,
}

/// A PersistentVolumeClaim: provisioned storage a workload reserved. Metrics-server does
/// not report filesystem usage INSIDE a volume (that needs Prometheus/kubelet stats), so
/// ccc reports capacity/status/class truthfully instead of inventing a percentage.
#[derive(Debug, Clone, PartialEq)]
pub struct KubePvc {
    pub namespace: String,
    pub name: String,
    pub phase: String,
    pub capacity_bytes: Option<u64>,
    pub class: String,
    pub volume: String,
}
