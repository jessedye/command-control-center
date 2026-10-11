//! Metric collection: local machine via `sysinfo`, remote hosts over the system `ssh`.
//!
//! Remote stations reuse the operator's own OpenSSH configuration — keys, agents, jump
//! hosts, aliases — because we shell out to the installed `ssh` binary rather than
//! reimplementing SSH. OpenSSH connection multiplexing (ControlMaster/ControlPersist)
//! makes each poll after the first ~free.

use crate::format;
use crate::gpu;
use crate::proto::{self, NetBaseline};
use crate::snapshot::{DiskRow, NetRow, ProcRow, Station};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// The CCC/1 collector shipped to remote hosts via ssh stdin (POSIX sh, zero install).
pub const COLLECTOR_SH: &str = include_str!("../assets/collector.sh");

pub fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bounded external commands (review fix #7)
// ---------------------------------------------------------------------------

/// Per-stream output cap for external commands. A chatty remote (motd floods, a kernel
/// dmesg dump over a serial console) must not exhaust the dashboard's memory before its
/// own deadline fires — readers stop at this size and close the pipe, which SIGPIPEs the
/// child (round-2 finding 3).
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Set once when the UI returns; every in-flight `run_bounded` polls it and kills its
/// child immediately, so quitting exits within one poll tick instead of waiting out a
/// 25 s ssh timeout per station (round-2 finding 22).
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Children whose deadline expired without a reap (kill sent, process stuck in D state).
/// Held here so the next `run_bounded` harvests them; bounded so we can never accumulate.
static DEFERRED_CHILDREN: Mutex<Vec<(Instant, Child)>> = Mutex::new(Vec::new());

fn kill_group(pid: u32) {
    // Negative-pid semantics: signal the whole process group created by `process_group(0)`.
    // Killing only the direct child would leak grandchild processes holding the pipes open
    // (round-2 findings 1/2: ssh spawns children; nvidia-smi stalls in driver calls).
    unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
}

/// Reap everything from previous calls that has since exited; abandon entries older than
/// 30 s (killed once more, then dropped — init reaps them once their syscall returns).
fn reap_deferred_children() {
    if let Ok(mut z) = DEFERRED_CHILDREN.lock() {
        let now = Instant::now();
        let mut i = 0;
        while i < z.len() {
            // Index access keeps the &mut Child borrow of try_wait() temporary; a retain
            // closure destructures it behind a shared reference and won't compile.
            let keep = match z[i].1.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) if now.duration_since(z[i].0).as_secs() > 30 => {
                    kill_group(z[i].1.id());
                    false
                }
                _ => true,
            };
            if keep {
                i += 1;
            } else {
                z.remove(i);
            }
        }
    }
}

fn defer_reap(child: Child) {
    if let Ok(mut z) = DEFERRED_CHILDREN.lock() {
        if z.len() >= 64 {
            kill_group(child.id());
            return; // saturated registry: drop our handle, init adopts the zombie
        }
        z.push((Instant::now(), child));
    }
}

/// Run an external command with a hard deadline, draining pipes while it runs.
///
/// A hung child (`ssh` session whose far end stops producing output, a wedged `kubectl`,
/// a stalled `nvidia-smi`) must never occupy a sampler thread forever: on timeout the
/// child's whole process group is killed, reaping is deadline-bounded (never a blocking
/// `wait()` on a task stuck in D state — round-2 finding 2), and callers get an error.
/// Reading stdout/stderr on dedicated threads prevents the classic pipe-full deadlock, and
/// each stream is capped at `MAX_OUTPUT_BYTES` (finding 3). `SHUTDOWN` short-circuits the
/// wait so quitting does not orphan live ssh/kubectl children (finding 22).
pub fn run_bounded(
    program: &str,
    args: &[String],
    stdin_data: Option<&[u8]>,
    timeout: Duration,
) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
    reap_deferred_children();
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so kill_group reaches descendants.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn '{program}' failed: {e} (is openssh-client installed?)"))?;
    let pgid = child.id();

    if let Some(data) = stdin_data {
        // Write on a helper thread so a full pipe buffer cannot deadlock us against the child.
        let mut stdin = child.stdin.take().expect("piped stdin");
        let payload = data.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&payload);
            // Dropping stdin here closes the pipe, which is how remote `sh -s` sees EOF.
        });
    }

    let out_rx = spawn_reader(child.stdout.take().expect("piped stdout"));
    let err_rx = spawn_reader(child.stderr.take().expect("piped stderr"));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                kill_group(pgid);
                defer_reap(child);
                return Err(format!("'{program}' wait failed: {e}"));
            }
        }
        if SHUTDOWN.load(Ordering::Relaxed) {
            kill_group(pgid);
            defer_reap(child);
            return Err(format!("'{program}' cancelled (app shutting down)"));
        }
        if Instant::now() >= deadline {
            kill_group(pgid);
            // Graceful bounded reap: give the SIGKILLed child a moment, then hand it to the
            // registry if the kernel will not reap it promptly. Never a blocking wait.
            let grace = Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < grace => thread::sleep(Duration::from_millis(20)),
                    _ => {
                        defer_reap(child);
                        break;
                    }
                }
            }
            return Err(format!("'{program}' timed out after {:.1}s", timeout.as_secs_f64()));
        }
        thread::sleep(Duration::from_millis(20));
    };
    // The child exited on its own, so both readers hit EOF quickly; keep a finite join budget.
    let (stdout, out_over) = out_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("'{program}' stdout read failed: {e}"))?;
    let (stderr, err_over) = err_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("'{program}' stderr read failed: {e}"))?;
    if out_over {
        return Err(format!("'{program}' exceeded {MAX_OUTPUT_BYTES} bytes of stdout (output truncated)"));
    }
    if err_over {
        return Err(format!("'{program}' exceeded {MAX_OUTPUT_BYTES} bytes of stderr (output truncated)"));
    }
    Ok((status.code(), stdout, stderr))
}

fn spawn_reader(pipe: impl ReadPipe + 'static) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // Bounded chunked reads: past MAX_OUTPUT_BYTES we stop and DROP the read-end,
        // so the still-running child gets EPIPE/SIGPIPE. The vec is capped; memory is flat.
        let mut pipe = pipe;
        let mut buf: Vec<u8> = Vec::new();
        let mut overflow = false;
        let mut chunk = [0u8; 16384];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_OUTPUT_BYTES {
                        let room = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                        buf.extend_from_slice(&chunk[..room]);
                        overflow = true;
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((buf, overflow));
        // `pipe` drops here: closing the fd is what SIGPIPEs an over-cap child.
    });
    rx
}

/// Small sealed trait so `run_bounded` accepts only owned pipe handles.
pub trait ReadPipe: std::io::Read + Send {}
impl ReadPipe for ChildStdout {}
impl ReadPipe for ChildStderr {}

/// A sampler that fills one UI slot repeatedly.
pub enum Source {
    Local(LocalCollector),
    Remote(RemoteCollector),
}

impl Source {
    pub fn sample(&mut self) -> Result<Station, String> {
        match self {
            Source::Local(l) => Ok(l.sample()),
            Source::Remote(r) => r.sample(),
        }
    }
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

const SSH_TIMEOUT: Duration = Duration::from_secs(25);

pub struct LocalCollector {
    sys: System,
    /// One long-lived `Networks`: fresh construction zeroes the deltas (review fix #3 —
    /// `received()` on a newly built instance reports 0, making local traffic permanently
    /// blank). We keep the instance and read CUMULATIVE totals, diffing in NetBaseline.
    nets: sysinfo::Networks,
    primed: bool,
    net_base: NetBaseline,
    prev_at: Instant,
    /// Cached `/etc/passwd` uid -> username (parsed once per process).
    users: HashMap<String, String>,
}

impl LocalCollector {
    pub fn new() -> Self {
        let mut nets = sysinfo::Networks::new_with_refreshed_list();
        // Force a delta baseline so the first refresh() yields real counters.
        nets.refresh(true);
        Self {
            sys: System::new(),
            nets,
            primed: false,
            net_base: NetBaseline::new(),
            prev_at: Instant::now(),
            users: passwd_users(),
        }
    }

    /// Fields we actually display: cpu%, memory, command line (for the name/argv columns),
    /// and user id. Review fix #2: sysinfo's default `refresh_processes` never populates
    /// `cmd()`, so the kernel-thread filter dropped EVERY process and the table was empty.
    /// Round 2 finding 14: `ProcessRefreshKind::default()` keeps `tasks: true` (verified in
    /// sysinfo-0.39.6 src/common/system.rs), which lists every thread as its own process —
    /// the duplicate Unity rows in shipped screenshots are threads. Drop them.
    fn proc_kind() -> ProcessRefreshKind {
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_cmd(UpdateKind::Always)
            .with_user(UpdateKind::Always)
            .without_tasks()
    }

    /// First call measures a dedicated 900 ms window (sysinfo percents are deltas between
    /// refreshes; without this, a one-shot --once frame would report 0% everywhere).
    /// Later calls inherit the sampler loop's pacing.
    pub fn sample(&mut self) -> Station {
        if !self.primed {
            self.sys.refresh_cpu_all();
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                Self::proc_kind(),
            );
            // Seed the network baseline INSIDE the prime window (round-2 finding 15): an empty
            // NetBaseline makes peek() emit warmup zeros for every interface, so `--once`
            // permanently showed "NET ▼ 0 B/s" even while 1 GB/s was flowing.
            self.nets.refresh(true);
            let c0: Vec<(String, u64, u64)> = self
                .nets
                .iter()
                .map(|(name, data)| (name.clone(), data.total_received(), data.total_transmitted()))
                .collect();
            self.net_base.commit(&c0);
            self.prev_at = Instant::now();
            self.primed = true;
            std::thread::sleep(std::time::Duration::from_millis(900));
        }
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, Self::proc_kind());

        let cores: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let load = System::load_average();

        // Persistent Networks instance: refresh() advances cumulative counters we diff below.
        self.nets.refresh(true);
        let counters: Vec<(String, u64, u64)> = self
            .nets
            .iter()
            .map(|(name, data)| (name.clone(), data.total_received(), data.total_transmitted()))
            .collect();
        let dt = self.prev_at.elapsed().as_secs_f64();
        let rates = self.net_base.peek(dt, &counters);
        self.net_base.commit(&counters);
        self.prev_at = Instant::now();
        let ts = epoch_now();
        let net: Vec<NetRow> = rates
            .into_iter()
            .map(|r| NetRow { link_speed_mbps: link_speed_linux(&r.iface), ..r })
            .collect();

        let mem_total = self.sys.total_memory();
        // Skip kernel helper threads (argv-less, pegged at 0% by construction in practice):
        // the dashboard table is more useful showing workloads than [kworker/*].
        let mut all: Vec<ProcRow> = self
            .sys
            .processes()
            .values()
            .filter(|p| !p.cmd().is_empty())
            .map(|p| proc_row_local(p, mem_total, &self.users))
            .collect();
        // Review fix #16: the renderer can sort by MEM/PID, so keep the union of the top
        // CPU, MEM and PID candidates — truncating to CPU-top-first made `--top 40` under
        // a MEM sort unshowable for the very processes that sort should surface.
        all.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(a.pid.cmp(&b.pid)));
        let mut keep: Vec<u32> = Vec::new();
        collect_top(&all, PROC_KEEP, |p| p.cpu_pct as f64, &mut keep);
        collect_top(&all, PROC_KEEP, |p| p.mem_pct as f64, &mut keep);
        collect_top(&all, PROC_KEEP, |p| -(p.pid as f64), &mut keep);
        let procs: Vec<ProcRow> = all
            .into_iter()
            .filter(|p| keep.contains(&p.pid))
            .collect();

        Station {
            name: crate::format::short_host(&hostname()),
            ts_epoch: ts,
            os_name: distro_label(),
            kernel: uname_r(),
            uptime_s: System::uptime(),
            load: [load.one as f32, load.five as f32, load.fifteen as f32],
            core_count: cores.len(),
            cpu_avg_pct: self.sys.global_cpu_usage(),
            per_core_pct: cores,
            mem_total_kb: mem_total / 1024,
            mem_avail_kb: self.sys.available_memory() / 1024,
            swap_total_kb: self.sys.total_swap() / 1024,
            swap_free_kb: self.sys.free_swap() / 1024,
            disks: disks_local(),
            net,
            gpus: gpu::detect_local(),
            procs,
        }
    }
}

/// Max process rows retained per sort key; CLI clamps `--top` to 40, 48 leaves headroom.
const PROC_KEEP: usize = 48;

fn collect_top(all: &[ProcRow], n: usize, key: fn(&ProcRow) -> f64, out: &mut Vec<u32>) {
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.sort_by(|&a, &b| key(&all[b]).total_cmp(&key(&all[a])));
    for i in idx.into_iter().take(n) {
        let pid = all[i].pid;
        if !out.contains(&pid) {
            out.push(pid);
        }
    }
}

fn proc_row_local(
    p: &sysinfo::Process,
    mem_total_bytes: u64,
    users: &HashMap<String, String>,
) -> ProcRow {
    let rss_kb = p.memory() / 1024;
    let mem_pct = if mem_total_bytes > 0 {
        (rss_kb * 1024) as f64 * 100.0 / mem_total_bytes as f64
    } else {
        0.0
    } as f32;
    let name = p.name().to_string_lossy().into_owned();
    let cmd: String = {
        let args: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        if args.is_empty() {
            format!("[{name}]")
        } else {
            format::trunc(&args.join(" "), 56)
        }
    };
    // sysinfo owns the uid lookup (replaces hand-rolled /proc/status reads): user_id needs
    // with_user in the refresh kind, which proc_kind() requests. Uid derefs to the platform
    // id; stringifying it keys the cache uniformly across numeric uids and Windows SIDs.
    let user = match p.user_id().map(|u| u.to_string()) {
        Some(key) => users.get(&key).cloned().unwrap_or(key),
        None => "?".to_string(),
    };
    // run_time() is seconds since the process started, measured by sysinfo against the
    // refresh clock — the hand-computed boot math produced 0 elapsed (review fix #2).
    ProcRow {
        pid: p.pid().as_u32(),
        user: format::trunc(&user, 8),
        cpu_pct: p.cpu_usage(),
        mem_pct,
        rss_kb,
        elapsed_s: p.run_time(),
        cmd: format::trunc(&cmd, 56),
    }
}

/// `/etc/passwd` username cache keyed by uid string (numeric on Unix).
fn passwd_users() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(text) = std::fs::read_to_string("/etc/passwd") {
        for line in text.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 3 && !f[2].is_empty() {
                map.insert(f[2].to_string(), f[0].to_string());
            }
        }
    }
    map
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

fn uname_r() -> String {
    // Bounded: a stalled `uname` must not wedge the sampler.
    match run_bounded("uname", &["-r".to_string()], None, Duration::from_secs(5)) {
        Ok((_, out, _)) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(_) => String::new(),
    }
}

fn distro_label() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        let pretty = format::distro_name(&text);
        if !pretty.is_empty() {
            return pretty;
        }
    }
    uname_r()
}

const REAL_FSTYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "jfs", "reiserfs", "f2fs", "zfs", "vfat", "exfat",
    "ntfs", "ntfs3",
];
const MOUNT_PREFIX_SKIP: &[&str] = &[
    "/snap/",
    "/var/lib/docker/",
    "/var/lib/containers/",
    "/var/lib/kubelet/",
    "/run/k3s/",
];

fn disks_local() -> Vec<DiskRow> {
    // Fresh construction per tick (~sub-ms) — no mutexes, matches the low-overhead goal.
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut rows: Vec<DiskRow> = Vec::new();
    for d in disks.list() {
        let fstype = d.file_system().to_string_lossy().to_lowercase();
        if !REAL_FSTYPES.contains(&fstype.as_str()) {
            continue;
        }
        let mount = d.mount_point().to_string_lossy().to_string();
        if MOUNT_PREFIX_SKIP.iter().any(|p| mount.starts_with(p)) {
            continue;
        }
        let total = d.total_space();
        if total < 1024 * 1024 * 1024 {
            continue; // smaller than 1 GiB: bind-mount noise, not a capacity
        }
        rows.push(DiskRow {
            mount,
            fstype,
            device: d.name().to_string_lossy().into_owned(),
            total_bytes: total,
            // sysinfo exposes df's AVAILABLE semantics only; free-with-reserve isn't in the
            // API. DiskRow documents the consequence (see snapshot.rs).
            free_bytes: d.available_space(),
            avail_bytes: d.available_space(),
        });
    }
    // Round-2 finding 13: LOCAL volumes dedup by DEVICE node — two identical-size SSDs are
    // distinct capacities and must both show (the old fstype+size key silently hid one,
    // alerts included). Remote frames carry no device identity (CCC/1 disk rows are a fixed
    // quintuple), so those keep the legacy fstype+size collapse: on `df -Pk` output equal
    // size+type genuinely means the same filesystem mounted twice (/home and / bind-mounted
    // off one subvolume render as duplicate bars otherwise).
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<DiskRow> = Vec::new();
    for row in rows {
        let key = if row.device.is_empty() {
            format!("S {}|{}", row.fstype, row.total_bytes)
        } else {
            format!("D {}", row.device)
        };
        match seen.get(&key) {
            Some(idx) => {
                if row.mount.chars().count() < deduped[*idx].mount.chars().count() {
                    deduped[*idx] = row;
                }
            }
            None => {
                seen.insert(key, deduped.len());
                deduped.push(row);
            }
        }
    }
    deduped.sort_by(|a, b| b.pct().total_cmp(&a.pct()));
    deduped
}

fn link_speed_linux(iface: &str) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{iface}/speed")).ok()?;
    let v: i64 = text.trim().parse().ok()?;
    if v > 0 {
        Some(v as u32)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Remote (ssh)
// ---------------------------------------------------------------------------

pub struct RemoteCollector {
    /// See RemoteSpec::retry_ms (config `throttle_wait`).
    pub retry_ms: u64,
    pub label: String,
    /// Extra ssh argv (jump hosts, ports, identity files...).
    pub ssh_args: Vec<String>,
    pub host: String,
    /// Password file for `sshpass -f` (Proxmox root auth has no key by default). None =
    /// plain key-based ssh via BatchMode; Some switches to sshpass + accept-new host keys.
    pub pass_file: Option<String>,
    net_base: NetBaseline,
    prev_at: Instant,
}

/// One-shot remote execution shared by RemoteCollector and the PVE collector: runs `script`
/// on `host` through `ssh host sh -s`, stdin-piped, deadline-bounded. With `pass_file` set,
/// the invocation is wrapped in `sshpass -f FILE`.
///
/// CRITICAL pairing: password mode must NOT send `-o BatchMode=yes` — BatchMode disables
/// every interactive prompt, which includes the very password prompt sshpass exists to
/// answer; keeping it makes sshpass exit 5 ("no more passwords") instantly. Password mode
/// also sets StrictHostKeyChecking=accept-new so first contact with a discovered node can
/// never block on a yes/no prompt (the original BatchMode's purpose).
pub fn run_remote_script(
    script: &str,
    ssh_args: &[String],
    host: &str,
    pass_file: Option<&str>,
) -> Result<String, String> {
    let (program, mut args): (&str, Vec<String>) = match pass_file {
        Some(f) => (
            "sshpass",
            vec![
                "-f".to_string(),
                f.to_string(),
                "ssh".to_string(),
                "-T".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
            ],
        ),
        None => {
            let mut v = vec!["-T".to_string()];
            v.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
            ("ssh", v)
        }
    };
    // Connection multiplexing (owner screenshot 2026-10-11): a 9-tab fleet launch opens nine
    // concurrent handshakes per tick; Proxmox sshd's default MaxStartups (~10 unauthenticated)
    // drops the surplus with `kex_exchange_identification: Connection closed`. A persisted
    // control master collapses every sample per host onto ONE connection after the first.
    // %C hashes host+port+user so no socket name exceeds the 104-char sun_path limit.
    // XDG_RUNTIME_DIR (not $HOME): same choice as RemoteCollector::new, so pve seeds and
    // station samplers share one control socket per host. The dir lives on tmpfs, so no
    // HOME dependency and no stale sockets surviving reboots.
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_e| "/tmp".to_string());
    if std::fs::create_dir_all(&dir).is_ok() {
        args.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            format!("ControlPath={dir}/ccc-ssh-%C"),
            "-o".to_string(),
            "ControlPersist=300".to_string(),
        ]);
    }
    // ConnectTimeout applies to ssh in both modes (it rides inside sshpass's ssh argv as a
    // passthrough option because anything after `ssh` below is ssh's argv).
    args.extend([
        "-o".to_string(),
        "ConnectTimeout=8".to_string(),
    ]);
    args.extend(ssh_args.iter().cloned());
    args.push(host.to_string());
    args.push("sh -s".to_string());

    let (code, stdout, stderr) = run_bounded(program, &args, Some(script.as_bytes()), SSH_TIMEOUT)?;
    match code {
        Some(0) => Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Some(c) => {
            let tail = format::trunc(String::from_utf8_lossy(&stderr).trim(), 160);
            Err(format!("{program} exited {c} {tail}"))
        }
        None => Err("ssh killed by signal".to_string()),
    }
}

/// Signatures of server-side login throttling (PAM faillock / OpenSSH connection caps):
/// reset-then-retry immediately makes these WORSE — every attempt re-extends the window.
/// Callers get exactly one slow retry; rapid loops are explicitly not the cure.
pub fn auth_throttled(msg: &str) -> bool {
    msg.contains("kex_exchange_identification")
        || msg.contains("Connection closed by")
        || msg.contains("Connection reset by")
}

/// Seconds of grace the fleet waits by default when a node answers with login throttling.
pub const DEFAULT_THROTTLE_WAIT_MS: u64 = 3500;

impl RemoteCollector {
    pub fn new(label: String, host: String, mut ssh_args: Vec<String>, pass_file: Option<String>, retry_ms: u64) -> Self {
        // Multiplex: first connection pays the handshake, later polls ride the socket.
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_e| "/tmp".to_string());
        ssh_args.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            format!("ControlPath={dir}/ccc-ssh-%C"),
            "-o".to_string(),
            "ControlPersist=300".to_string(),
        ]);
        Self { label, host, ssh_args, pass_file, net_base: NetBaseline::new(), prev_at: Instant::now(), retry_ms }
    }

    pub fn sample(&mut self) -> Result<Station, String> {
        let text = self.ssh_run(COLLECTOR_SH)?;
        let ts = epoch_now();
        // Round-2 finding 6: one strict parse yields BOTH the station and exactly the
        // cumulative counters that frame carried — no second pass over raw text, so bytes
        // after `end` can never merge into this sample.
        let proto::Frame { mut station, counters } = proto::parse(&text)
            .map_err(|e| format!("{}: {e}", self.label))?;
        // Rate window spans whole sampling cycles (ssh round-trip included), fractionally.
        let dt = self.prev_at.elapsed().as_secs_f64();
        station.net = self.net_base.peek(dt, &counters);
        // Commit the baseline only when the frame decoded (review fix #13): a corrupt frame
        // must not poison the next interval's counter baseline.
        self.net_base.commit(&counters);
        self.prev_at = Instant::now();
        if station.name.trim().is_empty() {
            station.name = self.label.clone();
        }
        station.ts_epoch = ts;
        for row in &mut station.net {
            row.link_speed_mbps = None; // remote sysfs not available; rate is the payload
        }
        Ok(station)
    }

    /// Run `script` through `ssh host sh -s`, returning stdout. Connection setup failures
    /// surface with the exit code and stderr tail. Review fix #7: the whole invocation is
    /// deadline-bounded and its pipes drained. With pass_file set this goes through sshpass
    /// (see run_remote_script for why BatchMode must be off in that mode).
    fn ssh_run(&self, script: &str) -> Result<String, String> {
        let first = run_remote_script(script, &self.ssh_args, &self.host, self.pass_file.as_deref());
        match first {
            Err(e) if auth_throttled(&e) => {
                // One slow retry only. Server-side login throttling EXTENDS the lockout on
                // every rapid attempt (observed live on PVE nodes); rapid looping makes the
                // station permanently red. The next sampling tick is the natural second chance.
                std::thread::sleep(std::time::Duration::from_millis(self.retry_ms.max(250)));
                run_remote_script(script, &self.ssh_args, &self.host, self.pass_file.as_deref())
            }
            other => other,
        }
    }
}

/// Resolve `~/.ssh/config`-style hostnames is ssh's job; here we only classify a
/// `-o`-prefixed token stream into (flags..., hostname).
///
/// Review fix #19: `-F` (config file) takes a value too; without it, `-F conf box` parsed
/// `conf` as the destination.
pub fn split_ssh_tokens(tokens: &[String]) -> Result<(Vec<String>, String), String> {
    let mut flags = Vec::new();
    let mut host: Option<String> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t.starts_with('-') {
            // Options that carry a separated value (`-p 2222`, `-F config`) consume the next
            // token. Round-2 finding 20: complete against OpenSSH's man page — `-D` socks,
            // `-R` reverse forward, `-W` stdio relay, `-I` pkcs11, etc. all take arguments;
            // a missing entry re-parses the VALUE as the hostname (silent misrouting).
            // Grouped shorts (`-46C`) are not expanded: ssh itself parses them at runtime.
            let takes_value = matches!(
                t,
                "-b" | "-B" | "-c" | "-D" | "-E" | "-e" | "-F" | "-I" | "-i" | "-J" | "-l"
                    | "-L" | "-m" | "-O" | "-o" | "-p" | "-Q" | "-R" | "-S" | "-W" | "-w"
            );
            flags.push(t.to_string());
            i += 1;
            if takes_value {
                let v = tokens
                    .get(i)
                    .ok_or_else(|| format!("missing value for ssh option '{t}'"))?;
                flags.push(v.clone());
                i += 1;
            }
        } else if host.is_none() {
            host = Some(t.to_string());
            i += 1;
        } else {
            return Err(format!(
                "unexpected argument '{t}' after host '{}'",
                host.as_deref().unwrap()
            ));
        }
    }
    match host {
        Some(h) => Ok((flags, h)),
        None => Err("no destination hostname found".to_string()),
    }
}

/// Default label for a remote spec: the LAST token that looks like a destination.
/// Option-value pairs (`-p 2222`, `-i /path/key`) must not be mistaken for a host,
/// and ssh specs put the hostname at the end.
pub fn auto_label(tokens: &[String]) -> String {
    for t in tokens.iter().rev() {
        let usable = !t.starts_with('-')
            && !t.chars().all(|c| c.is_ascii_digit())
            && !t.starts_with('/')
            && !t.contains('=');
        if usable {
            return format::short_host(t);
        }
    }
    "remote".to_string()
}

/// Quote-aware whitespace splitter for `-H "NAME=-p 2222 user@box"`-style specs so quoted
/// arguments survive (review fix #19): `-i "my key"` stays ONE token, single quotes too.
/// Backslash escapes the next character inside double quotes, matching shell basics.
pub fn shell_split(spec: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false; // current word exists even if empty ("")
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                has = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("unterminated single quote".to_string()),
                    }
                }
            }
            '"' => {
                has = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => {
                            cur.push(chars.next().ok_or("unterminated escape")?);
                        }
                        Some(x) => cur.push(x),
                        None => return Err("unterminated double quote".to_string()),
                    }
                }
            }
            '\\' => {
                has = true;
                cur.push(chars.next().ok_or("trailing backslash")?);
            }
            c if c.is_whitespace() => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            x => {
                has = true;
                cur.push(x);
            }
        }
    }
    if has {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_flags_from_host() {
        let t: Vec<String> = ["-p", "2222", "-i", "/home/x/.ssh/id_ed25519", "box.example"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (flags, host) = split_ssh_tokens(&t).unwrap();
        assert_eq!(host, "box.example");
        assert_eq!(flags, vec!["-p", "2222", "-i", "/home/x/.ssh/id_ed25519"]);
        assert_eq!(auto_label(&t), "box");
    }

    #[test]
    fn rejects_trailing_junk() {
        let t: Vec<String> = ["host", "extra"].iter().map(|s| s.to_string()).collect();
        assert!(split_ssh_tokens(&t).is_err());
    }

    #[test]
    fn missing_option_value_errors() {
        let t: Vec<String> = ["-p"].iter().map(|s| s.to_string()).collect();
        assert!(split_ssh_tokens(&t).is_err());
    }

    #[test]
    fn config_file_option_consumes_value() {
        // Review fix #19: -F takes the config file; the NEXT bare token is the host.
        let t: Vec<String> = ["-F", "site.conf", "box.example"].iter().map(|s| s.to_string()).collect();
        let (flags, host) = split_ssh_tokens(&t).unwrap();
        assert_eq!(host, "box.example");
        assert_eq!(flags, vec!["-F", "site.conf"]);
    }

    #[test]
    fn shell_split_preserves_quotes() {
        let w = shell_split("-i \"/tmp/my key\" user@box").unwrap();
        assert_eq!(w, vec!["-i", "/tmp/my key", "user@box"]);
        let w = shell_split("label=-p 2222 'host name'").unwrap();
        assert_eq!(w[0], "label=-p"); // label form handled upstream; quoting intact
        assert_eq!(shell_split("'unbalanced").unwrap_err(), "unterminated single quote");
        let w = shell_split("a\\ b c").unwrap();
        assert_eq!(w, vec!["a b", "c"]);
    }

    #[test]
    fn disk_pct_matches_df_convention() {
        // GNU df-style root-reserve accounting: used/(used+avail). See snapshot.rs pct().
        let d = DiskRow {
            mount: "/".into(),
            fstype: "ext4".into(),
            device: "/dev/sda1".into(),
            total_bytes: 1000,
            free_bytes: 300,
            avail_bytes: 200,
        };
        // df divides by used+avail so root-reserved blocks read as full-capacity pressure:
        // 700 used against 200 user-available => 77.8%, not 70%.
        assert!((d.pct() - 700.0 * 100.0 / 900.0).abs() < 1e-6);
        let zero = DiskRow {
            mount: String::new(),
            fstype: String::new(),
            device: String::new(),
            total_bytes: 0,
            free_bytes: 0,
            avail_bytes: 0,
        };
        assert_eq!(zero.pct(), 0.0);
    }

    #[test]
    fn bounded_command_caps_flooding_output() {
        // Round-2 finding 3: `yes` emits unbounded stdout; run_bounded must stop it at the
        // cap and report an error instead of growing the buffer forever.
        let e = run_bounded(
            "sh",
            &["-c".into(), "yes x".into()],
            None,
            Duration::from_secs(30),
        )
        .expect_err("a flooding child must be cut off");
        assert!(e.contains("exceeded"), "message: {e}");
    }

    #[test]
    fn bounded_command_enforces_deadline() {
        // A command that outlives its budget is killed and surfaces as a timeout error.
        let e = run_bounded("sleep", &[120.to_string()], None, Duration::from_millis(200))
            .expect_err("a hung child must surface as an error");
        assert!(e.contains("timed out"), "message: {e}");
    }

    #[test]
    fn bounded_command_captures_output_and_stdin() {
        let (code, out, err) = run_bounded(
            "sh",
            &["-c".into(), "printf hello; echo E >&2".into()],
            Some(b"hi"),
            Duration::from_secs(5),
        )
        .expect("completes");
        assert_eq!(code, Some(0));
        assert_eq!(String::from_utf8_lossy(&out), "hello");
        assert_eq!(String::from_utf8_lossy(&err), "E\n");
    }
}

#[cfg(test)]
mod throttle_tests {
    use super::auth_throttled;

    #[test]
    fn recognizes_server_side_login_throttle_signatures() {
        assert!(auth_throttled("sshpass exited 255 kex_exchange_identification: Connection closed by remote host"));
        assert!(auth_throttled("sshpass exited 255 Connection reset by 192.0.2.242 port 22"));
        assert!(auth_throttled("ssh exited 255 Connection closed by 10.0.0.9 port 22"));
        assert!(!auth_throttled("ssh exited 255 Permission denied, please try again."));
        assert!(!auth_throttled("ssh exited 255 Host key verification failed."));
        assert!(!auth_throttled("killed by signal"), "local timeout is not a throttle");
    }
}
