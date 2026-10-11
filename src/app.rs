//! Live dashboard loop: one sampler thread per station, a key-reader thread, and a
//! single-threaded renderer. No async runtime — std threads + mpsc keep the binary's
//! footprint tiny (the owner requirement: "very low overhead").

use crate::collect::{self, epoch_now, LocalCollector, RemoteCollector, Source};
use crate::format;
use crate::kube::{self, KubeCollector};
use crate::pve::{PveCollector, PveSpec};
use crate::render::{self, UiStation, View};
use crate::snapshot::{KubeNode, KubePod, KubePvc, PveCluster, Station};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::Paragraph;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum SortBy {
    Cpu,
    Mem,
    Pid,
}

#[derive(Clone)]
pub struct RemoteSpec {
    /// Milliseconds to wait before the single retry when a station hits server-side
    /// login throttling (config `throttle_wait`, see docs/SETUP.md §9).
    pub retry_ms: u64,
    pub label: String,
    pub host: String,
    pub ssh_args: Vec<String>,
    /// Password file for sshpass (shared fleet-wide via --pass / `pass` config setting).
    pub pass_file: Option<String>,
}

pub struct Config {
    pub interval: f64,
    pub top: usize,
    pub sort: SortBy,
    pub station_label: String,
    pub no_color: bool,
    pub verbose: bool,
    pub warn: f64,
    pub crit: f64,
    pub remotes: Vec<RemoteSpec>,
    /// Some(ctx) enables the k8s fleet tab with an optional kube context.
    pub kube: Option<String>,
    /// Display name for the fleet tab (config `k8s CONTEXT LABEL`). None = "K8S:{ctx}".
    pub kube_label: Option<String>,
    /// Proxmox clusters to watch, one tab per spec (`pve` / `-P`). One seed connection
    /// each yields nodes + guests + storage + ceph health. Password-auth specs carry
    /// their `pass_file` inside PveSpec/RemoteSpec — there is no fleet-wide password knob.
    pub pves: Vec<PveSpec>,
    /// Tab index to open focused. Label resolution happens in main; 0 = local station
    /// (or the overview slot when summary mode inserts one).
    pub focus: usize,
    /// Fleet-summary placement; `Overview` adds an "overview" tab at position 1 that owns
    /// the quick-stats strip (owner directive 2026-10-11: off every station screen).
    pub summary: crate::config::SummaryMode,
}

/// Tab bar name for the fleet slot, honoring the config display label.
/// Re-exported so main.rs can name the tab without reaching into render.
pub use crate::render::OVERVIEW_LABEL;

pub fn fleet_tab_name(kube: &Option<String>, kube_label: &Option<String>) -> String {
    match (kube_label, kube.as_deref()) {
        (Some(l), _) => format!("K8S:{l}"),
        (None, Some(ctx)) if !ctx.is_empty() => format!("K8S:{ctx}"),
        _ => "K8S CLUSTER".to_string(),
    }
}

/// Tab bar name for a Proxmox cluster. The seed host shortens to its hostname/IP tail
/// (`node1`, `192.0.2.241`) when no explicit label was given, so bare `-P root@host`
/// still yields a readable tab like `-H` does.
pub fn pve_tab_name(label: &str) -> String {
    format!("PVE:{label}")
}

enum Sample {
    Host(usize, Result<Station, String>),
    Fleet(usize, Result<kube::KubeFleet, String>),
    Pve(usize, Result<PveCluster, String>),
}

struct Slot {
    name: String,
    latest: Option<Station>,
    fleet: Option<Vec<KubeNode>>,
    /// Container usage rows from the fleet poll (empty on non-fleet slots / failed polls).
    pods: Vec<KubePod>,
    pvcs: Vec<KubePvc>,
    /// Proxmox cluster view for PVE slots (nodes + guests + storage + ceph health).
    pve: Option<PveCluster>,
    /// Why `kubectl top` produced no metrics (review fix #23) — surfaced on the fleet panel.
    note: Option<String>,
    err: Option<String>,
    hist_cpu: Vec<f64>,
    hist_mem: Vec<f64>,
    /// Unix seconds of the last SUCCESSFUL sample for this slot (0 = never). Drives the
    /// stale AGE stamp for fleet stations, which have no Station.ts_epoch (round-2 #9).
    ts: i64,
}

impl Slot {
    fn new(name: String) -> Self {
        Self {
            name,
            latest: None,
            fleet: None,
            pods: Vec::new(),
            pvcs: Vec::new(),
            pve: None,
            note: None,
            err: None,
            hist_cpu: Vec::new(),
            hist_mem: Vec::new(),
            ts: 0,
        }
    }
}

const SPIN: [char; 4] = ['|', '/', '-', '\\'];
const MAX_HIST: usize = 512;

pub fn run(cfg: Config) -> Result<(), String> {
    // NaN comparisons are false at any bound, so validate finiteness too (review fix #14):
    // `--interval NaN` used to pass the floor check and panic inside from_secs_f64.
    if !cfg.interval.is_finite() || cfg.interval < 0.25 {
        return Err("--interval must be a finite number >= 0.25 seconds".to_string());
    }
    let tick = Duration::from_secs_f64(cfg.interval);

    // Slot layout: 0 = local, 1..=remotes.len() = ssh stations, then the k8s fleet tab
    // (optional), then one tab per Proxmox cluster. Order matters: spawn_samplers assigns
    // thread sources by the same indices.
    let mut slots = vec![Slot::new(cfg.station_label.clone())];
    for r in &cfg.remotes {
        slots.push(Slot::new(r.label.clone()));
    }
    if cfg.kube.is_some() {
        // Empty context string (bare --k8s) means "current context" -> neutral label (#8).
        slots.push(Slot::new(fleet_tab_name(&cfg.kube, &cfg.kube_label)));
    }
    for pv in &cfg.pves {
        slots.push(Slot::new(pve_tab_name(&pv.label)));
    }
    // Overview tab (owner directive 2026-10-11): a slot at position 1 that owns the
    // fleet summary, so station tabs show only their own detail panels. It collects
    // nothing — spawn_samplers offsets every sampler by this flag; render keys off
    // render::OVERVIEW_LABEL.
    if cfg.summary != crate::config::SummaryMode::Off {
        slots.insert(0, Slot::new(render::OVERVIEW_LABEL.to_string()));
    }

    // Round-2 finding 23: a BOUNDED channel — an unbounded one queued every snapshot while
    // rendering stalled, growing memory and showing ever-staler frames. Capacity ~2 frames
    // per station: producers block briefly (throttle), never accumulate without bound.
    let (tx, rx) = mpsc::sync_channel::<Sample>(slots.len() * 2 + 4);
    let sampler_threads = spawn_samplers(&cfg, tx, tick);

    // Key reader on its own thread: crossterm event::read blocks until input arrives.
    // Review fix #1: `event::poll(Duration::MAX)` never woke after quit, so `reader.join()`
    // hung the process forever. A finite poll checked against a shared stop flag lets the
    // reader exit within 200 ms of the UI returning from ratatui::run.
    let (ktx, krx) = mpsc::channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if !event::poll(Duration::from_millis(200)).unwrap_or(false) {
                continue;
            }
            match event::read() {
                Ok(ev) => {
                    if ktx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        })
    };

    // Config.focus comes from main (label resolution against the tab list). Clamped here
    // so a malformed index can never panic inside the tab handler.
    let focus = cfg.focus.min(slots.len().saturating_sub(1));
    let mut app = App {
        slots,
        focus,
        sort: cfg.sort,
        top: cfg.top,
        verbose: cfg.verbose,
        paused: false,
        spin: 0,
        warn: cfg.warn,
        crit: cfg.crit,
        summary: cfg.summary,
        rx,
    };
    let result = ratatui::run(|terminal| app.loop_live(&cfg, terminal, &krx, tick));
    // Round-2 finding 22: quitting must not orphan in-flight ssh/kubectl/nvidia-smi children.
    // SHUTDOWN makes every run_bounded kill its child group within ~20 ms; joining drains
    // the sampler threads that are between samples. App drop closes rx, ending their loops.
    stop.store(true, Ordering::Relaxed);
    collect::SHUTDOWN.store(true, Ordering::Relaxed);
    reader.join().ok();
    for handle in sampler_threads {
        let _ = handle.join();
    }
    result
}

/// What feeds each slot index (mirrors the slot layout built in run()).
enum SlotSource {
    Local,
    Remote(RemoteSpec),
    Kube,
    Pve(PveSpec),
}

fn slot_sources(cfg: &Config, count: usize) -> Vec<SlotSource> {
    let mut v = Vec::with_capacity(count);
    for idx in 0..count {
        v.push(if idx == 0 {
            SlotSource::Local
        } else if idx - 1 < cfg.remotes.len() {
            SlotSource::Remote(cfg.remotes[idx - 1].clone())
        } else {
            let i = idx - 1 - cfg.remotes.len();
            if cfg.kube.is_some() && i == 0 {
                SlotSource::Kube
            } else {
                SlotSource::Pve(cfg.pves[i - usize::from(cfg.kube.is_some())].clone())
            }
        });
    }
    v
}

fn spawn_samplers(cfg: &Config, tx: SyncSender<Sample>, tick: Duration) -> Vec<thread::JoinHandle<()>> {
    let mut handles = Vec::new();
    // Slot-0 overview tab (summary Overview mode) shifts every sampler's destination slot
    // right by one; stagger() keeps the raw index so first-contact spacing is unchanged.
    let off = usize::from(cfg.summary != crate::config::SummaryMode::Off);
    for (raw_idx, src) in slot_sources(cfg, cfg.remotes.len() + 1 + usize::from(cfg.kube.is_some()) + cfg.pves.len())
        .into_iter()
        .enumerate()
    {
        let idx = raw_idx + off;
        let tx_i = tx.clone();
        match src {
            SlotSource::Kube => {
                let collector = KubeCollector { context: cfg.kube.clone().filter(|c| !c.is_empty()) };
                handles.push(thread::spawn(move || {
                    stagger(idx, &tick);
                    loop {
                        let at = Instant::now() + tick.max(Duration::from_millis(200));
                        let msg = match collector.sample() {
                            Ok(fleet) => Sample::Fleet(idx, Ok(fleet)),
                            Err(e) => Sample::Fleet(idx, Err(e)),
                        };
                        if tx_i.send(msg).is_err() {
                            return;
                        }
                        sleep_until(at);
                    }
                }));
            }
            SlotSource::Pve(spec) => {
                let collector = PveCollector::new(spec);
                handles.push(thread::spawn(move || {
                    stagger(idx, &tick);
                    loop {
                        // A pve poll runs ~4 pvesh calls over one ssh hop: give it the
                        // same floor as kubectl (live sampling on the far side).
                        let at = Instant::now() + tick.max(Duration::from_millis(200));
                        let msg = match collector.sample() {
                            Ok(cluster) => Sample::Pve(idx, Ok(cluster)),
                            Err(e) => Sample::Pve(idx, Err(e)),
                        };
                        if tx_i.send(msg).is_err() {
                            return;
                        }
                        sleep_until(at);
                    }
                }));
            }
            SlotSource::Local | SlotSource::Remote(_) => {
                handles.push(thread::spawn(move || {
                    let mut source = match src {
                        SlotSource::Remote(r) => {
                            Source::Remote(RemoteCollector::new(r.label, r.host, r.ssh_args, r.pass_file, r.retry_ms))
                        }
                        _ => Source::Local(LocalCollector::new()),
                    };
                    stagger(idx, &tick);
                    loop {
                        let at = Instant::now() + tick;
                        let sample = source.sample();
                        let msg = match sample {
                            Ok(st) => Sample::Host(idx, Ok(st)),
                            Err(e) => Sample::Host(idx, Err(e)),
                        };
                        if tx_i.send(msg).is_err() {
                            return; // UI thread gone
                        }
                        sleep_until(at);
                    }
                }));
            }
        }
    }
    drop(tx); // main copy goes away; samplers exit when App drops rx
    handles
}

/// Spread initial samples so N stations don't handshake all at once.
fn stagger(idx: usize, tick: &Duration) {
    // Password handshakes are slow (~1-2 s), so a fleet launch firing every station at once
    // keeps >10 unauthenticated connections alive per sshd and OpenSSH answers kex with
    // "Not allowed at this time" (observed live 2026-10-11 on the PVE nodes). Space first
    // contacts ~400 ms apart; ControlMaster persists the master after that, so steady state
    // opens no new sockets. Cap keeps a huge roster from starving late tabs for minutes.
    thread::sleep((*tick / 5).min(Duration::from_millis(700)) * idx as u32);
}

fn sleep_until(at: Instant) {
    let d = at.saturating_duration_since(Instant::now());
    if d.is_zero() {
        thread::sleep(Duration::from_millis(5));
    } else {
        thread::sleep(d);
    }
}

struct App {
    slots: Vec<Slot>,
    focus: usize,
    sort: SortBy,
    top: usize,
    verbose: bool,
    paused: bool,
    spin: usize,
    warn: f64,
    crit: f64,
    summary: crate::config::SummaryMode,
    rx: Receiver<Sample>,
}

impl App {
    fn loop_live(
        &mut self,
        cfg: &Config,
        terminal: &mut ratatui::DefaultTerminal,
        krx: &Receiver<Event>,
        _tick: Duration,
    ) -> Result<(), String> {
        loop {
            // Keys get first priority (instant feel); draws pace on samples + 100ms clock.
            match krx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        KeyCode::Char('s') => {
                            self.sort = match self.sort {
                                SortBy::Cpu => SortBy::Mem,
                                SortBy::Mem => SortBy::Pid,
                                SortBy::Pid => SortBy::Cpu,
                            };
                        }
                        // Finding 29: floor of 1 matches main.rs's `--top` clamp (was 2 live,
                        // contradicting "--top 1" accepted on the command line).
                        KeyCode::Char('+') | KeyCode::Char('=') => self.top = (self.top + 1).clamp(1, 40),
                        KeyCode::Char('-') => self.top = self.top.saturating_sub(1).max(1),
                        KeyCode::Char('v') => self.verbose = !self.verbose,
                        KeyCode::Char(' ') => self.paused = !self.paused,
                        KeyCode::Tab | KeyCode::Right => self.focus = (self.focus + 1) % self.slots.len(),
                        KeyCode::Left | KeyCode::Backspace => {
                            self.focus = (self.focus + self.slots.len() - 1) % self.slots.len();
                        }
                        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                            let i = c as usize - '1' as usize;
                            if i < self.slots.len() {
                                self.focus = i;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
            }

            // Drain every queued sample, then draw exactly one frame.
            while let Ok(sample) = self.rx.try_recv() {
                if !self.paused {
                    self.apply(sample);
                }
            }
            self.spin = (self.spin + 1) % SPIN.len();

            // The owner's missing-rails screenshots traced to THIS: ratatui draws widgets
            // into the viewport AREA cached at startup — size() alone refreshes the dimension
            // numbers used for layout math, while f.area() kept serving the launch-time box.
            // A window resized/maximized after `ccc` launched therefore clipped every
            // right-aligned gauge off-screen while the left-side panels grew normally.
            // auto_crop() re-syncs viewport_area to the live backend size every frame.
            terminal.autoresize().map_err(|e| format!("crop failed: {e}"))?;
            let size = terminal.size().map_err(|e| format!("size query failed: {e}"))?;
            let view = self.view(size.width, size.height);
            let lines = render::to_ratatui(&render::build_rows(&view), cfg.no_color);
            terminal
                .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
                .map_err(|e| format!("draw failed: {e}"))?;
        }
    }

    fn apply(&mut self, sample: Sample) {
        // Review fix #6: a failed sample sets `err` WITHOUT dropping `latest`/`fleet`, so the
        // last known-good data stays on screen (stamped STALE by the renderer) for minutes.
        let slot = match sample {
            Sample::Host(i, Ok(st)) => self.slot_mut(i).map(|s| {
                push_hist(&mut s.hist_cpu, st.cpu_avg_pct as f64);
                push_hist(&mut s.hist_mem, st.mem_pct());
                s.ts = st.ts_epoch;
                s.latest = Some(st);
                s.err = None;
            }),
            Sample::Host(i, Err(e)) => self.slot_mut(i).map(|s| s.err = Some(e)),
            Sample::Fleet(i, Ok(fleet)) => self.slot_mut(i).map(|s| {
                // Finding 33: blank notes must stay None so the renderer's explanatory
                // "metrics-server reports no metrics…" fallback applies.
                s.note = join_notes(&fleet.notes);
                s.ts = epoch_now();
                s.pods = fleet.pods;
                s.pvcs = fleet.pvcs;
                s.fleet = Some(fleet.nodes);
                s.err = None;
            }),
            Sample::Fleet(i, Err(e)) => self.slot_mut(i).map(|s| {
                s.err = Some(format!("kubectl: {e}"));
            }),
            // PVE mirrors the fleet handling: keep the last good cluster on a failed poll
            // (stale-stamped) rather than blanking the panel.
            Sample::Pve(i, Ok(cluster)) => self.slot_mut(i).map(|s| {
                s.ts = epoch_now();
                s.pve = Some(cluster);
                s.err = None;
            }),
            Sample::Pve(i, Err(e)) => self.slot_mut(i).map(|s| {
                s.err = Some(format!("pvesh: {e}"));
            }),
        };
        let _ = slot;
    }

    fn slot_mut(&mut self, i: usize) -> Option<&mut Slot> {
        self.slots.get_mut(i)
    }

    fn view(&self, width: u16, height: u16) -> View<'_> {
        let stations = self
            .slots
            .iter()
            .map(|s| UiStation {
                label: s.name.clone(),
                host: s.latest.as_ref(),
                err: s.err.clone(),
                kube: s.fleet.as_deref(),
                kube_pods: &s.pods,
                kube_pvcs: &s.pvcs,
                kube_note: s.note.clone(),
                pve: s.pve.as_ref(),
                hist_cpu: s.hist_cpu.clone(),
                hist_mem: s.hist_mem.clone(),
                sample_ts: s.ts,
            })
            .collect();
        View {
            clock: format::utc_hms(epoch_now()),
            epoch: epoch_now(),
            spin: SPIN[self.spin],
            stations,
            focus: self.focus.min(self.slots.len().saturating_sub(1)),
            summary: self.summary,
            sort_label: sort_label(&self.sort),
            top: self.top,
            verbose: self.verbose,
            paused: self.paused,
            warn: self.warn_of(),
            crit: self.crit_of(),
            width,
            height,
        }
    }

    fn warn_of(&self) -> f64 { self.warn }
    fn crit_of(&self) -> f64 { self.crit }
}

fn push_hist(v: &mut Vec<f64>, x: f64) {
    v.push(x);
    if v.len() > MAX_HIST {
        v.remove(0);
    }
}

/// Join collector notes into one display string, dropping blanks (finding 33: an empty
/// joined note stored as Some("") suppresses the renderer's explanatory fallback).
fn join_notes(notes: &[String]) -> Option<String> {
    let kept: Vec<&str> = notes.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("; "))
    }
}

fn sort_label(s: &SortBy) -> &'static str {
    match s {
        SortBy::Cpu => "CPU",
        SortBy::Mem => "MEM",
        SortBy::Pid => "PID",
    }
}

// ---------------------------------------------------------------------------
// One-shot snapshot rendering (--once): synchronous, scriptable, README-friendly.
// ---------------------------------------------------------------------------

pub fn snapshot_text(cfg: &Config, width: u16, height: u16) -> String {
    let mut slots: Vec<Slot> = Vec::new();
    slots.push(Slot::new(cfg.station_label.clone()));
    let mut sources: Vec<Source> = vec![Source::Local(LocalCollector::new())];
    for r in &cfg.remotes {
        slots.push(Slot::new(r.label.clone()));
        sources.push(Source::Remote(RemoteCollector::new(
            r.label.clone(),
            r.host.clone(),
            r.ssh_args.clone(),
            r.pass_file.clone(),
            r.retry_ms,
        )));
    }
    if cfg.kube.is_some() {
        slots.push(Slot::new(fleet_tab_name(&cfg.kube, &cfg.kube_label)));
    }
    for pv in &cfg.pves {
        slots.push(Slot::new(pve_tab_name(&pv.label)));
    }

    // Each sample takes ~1s by design (the collector's sampling window); sequential is fine.
    for (idx, src) in sources.into_iter().enumerate() {
        let mut src = src;
        match src.sample() {
            Ok(st) => {
                slots[idx].ts = st.ts_epoch;
                slots[idx].latest = Some(st);
            }
            Err(e) => slots[idx].err = Some(e),
        }
    }
    if cfg.kube.is_some() {
        let ki = slots.len() - 1 - cfg.pves.len();
        let collector = KubeCollector { context: cfg.kube.clone().filter(|c| !c.is_empty()) };
        match collector.sample() {
            Ok(fleet) => {
                slots[ki].note = join_notes(&fleet.notes);
                slots[ki].ts = epoch_now();
                slots[ki].fleet = Some(fleet.nodes);
                slots[ki].pods = fleet.pods;
                slots[ki].pvcs = fleet.pvcs;
            }
            Err(e) => slots[ki].err = Some(format!("kubectl: {e}")),
        }
    }
    for (k, spec) in cfg.pves.iter().enumerate() {
        let si = slots.len() - cfg.pves.len() + k;
        let collector = PveCollector::new(spec.clone());
        match collector.sample() {
            Ok(cluster) => {
                slots[si].ts = epoch_now();
                slots[si].pve = Some(cluster);
            }
            Err(e) => slots[si].err = Some(format!("pvesh: {e}")),
        }
    }

    // Mirror run()'s layout AFTER sampling so indices above stay station-relative.
    if cfg.summary != crate::config::SummaryMode::Off {
        slots.insert(0, Slot::new(render::OVERVIEW_LABEL.to_string()));
    }

    let stations: Vec<UiStation> = slots
        .iter()
        .map(|s| UiStation {
            label: s.name.clone(),
            host: s.latest.as_ref(),
            err: s.err.clone(),
            kube: s.fleet.as_deref(),
            kube_pods: &s.pods,
            kube_pvcs: &s.pvcs,
            kube_note: s.note.clone(),
            pve: s.pve.as_ref(),
            hist_cpu: Vec::new(),
            hist_mem: Vec::new(),
            sample_ts: s.ts,
        })
        .collect();

    let focus = cfg.focus.min(stations.len().saturating_sub(1));
    let view = View {
        clock: format::utc_hms(epoch_now()),
        epoch: epoch_now(),
        spin: '*',
        stations,
        focus,
        summary: cfg.summary,
        sort_label: sort_label(&cfg.sort),
        top: cfg.top.max(1),
        verbose: cfg.verbose,
        paused: false,
        warn: cfg.warn,
        crit: cfg.crit,
        width,
        height,
    };
    render::to_plain(&render::build_rows(&view)).join("\n")
}
