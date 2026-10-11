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

/// One Proxmox VE hypervisor node, from `pvesh get /cluster/status` (name, ip, online)
/// joined with `/cluster/resources --type node` (cpu, mem, uptime). All fields arrive in
/// ONE ssh round-trip on the seed node — no per-node login needed for the cluster view.
#[derive(Debug, Clone, PartialEq)]
pub struct PveNode {
    pub name: String,
    pub ip: String,
    /// True when the cluster reports the node online; offline nodes carry no live cpu/mem.
    pub online: bool,
    /// Percent of the WHOLE host's cores (PVE semantics), 0..100. None = not reported.
    pub cpu_pct: Option<f64>,
    pub max_cpu: u64,
    pub mem_bytes: u64,
    pub max_mem_bytes: u64,
    pub uptime_s: u64,
}

/// One VM or container on the cluster (`/cluster/resources --type vm`). Kubernetes exposes
/// pods; Proxmox exposes guests — this is the hypervisor-level analog of TOP PODS.
#[derive(Debug, Clone, PartialEq)]
pub struct PveGuest {
    pub vmid: u32,
    /// "qemu" (VM) or "lxc" (container), verbatim from the type column.
    pub kind: String,
    pub name: String,
    pub node: String,
    /// running | stopped | paused | ...; empty when the node is offline and knows nothing.
    pub status: String,
    pub template: bool,
    /// Percent of the guest's OWN allocated cores, normalized by PVE against vCPU count.
    pub cpu_pct: Option<f64>,
    pub max_cpu: u64,
    pub mem_bytes: u64,
    pub max_mem_bytes: u64,
}

/// One storage attachment as `pvesh get /nodes/<n>/storage` reports it: dir/NFS/LVM-thin/
/// CephFS/RBD pools seen from that node. Usage figures are PER NODE (the same NFS export
/// mounts everywhere); `used_pct` comes straight from PVE's own used_fraction column.
#[derive(Debug, Clone, PartialEq)]
pub struct PveStorage {
    pub node: String,
    pub name: String,
    /// dir | nfs | lvmthin | lvm | zfspool | cephfs | rbd | … (type column verbatim).
    pub kind: String,
    /// Content types the storage accepts ("images,rootdir",…, truncated at display time.
    pub content: String,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub used_bytes: u64,
    /// Percent full per PVE's used_fraction; None on offline/inactive storages.
    pub used_pct: Option<f64>,
}

/// Full snapshot of one Proxmox cluster from a single seed-node connection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PveCluster {
    pub nodes: Vec<PveNode>,
    pub guests: Vec<PveGuest>,
    pub storage: Vec<PveStorage>,
    /// Health summary parsed from `pvesh get /cluster/ceph/health` — "OK"/"WARN"/
    /// "HEALTH_WARN"/… Unparsable sections land in `notes` instead, never here.
    pub ceph_health: Option<String>,
    /// Human-readable collection issues (missing pvesh, unparsable sections…). Surfaced
    /// on the panel like kube notes — never silently dropped.
    pub notes: Vec<String>,
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
