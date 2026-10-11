//! Presentation layer: turns UI state into styled `ratatui` lines (live mode) or plain
//! text (`--once`, README samples, tests). One line pipeline serves both paths, so what
//! you unit-test is what ships.
//!
//! Visual language follows the approved concept sheet (Codex Banner B): heavy C2 stencil
//! wordmark, segmented gauges with quarter ticks, per-core vertical strips, `[o]/[!]/[X]`
//! status lamps, `>>  WARN ... <<` chevrons, footer with station/uptime/load/clock/alerts.

use crate::format;
use crate::snapshot::{KubeNode, KubePod, KubePvc, Station};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Every style token maps to one palette entry; `no_color` collapses styling for `--once`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St {
    Normal,
    Ident, // cyan identity
    Title, // bold cyan
    Dim,   // chrome/rules
    Rule,  // dark separators
    Ok,
    Warn,
    Crit,
    Head, // column headers
}

const RGB: &[(u8, u8, u8)] = &[
    (0, 0, 0),          // Normal -> default fg
    (0, 215, 255),      // Ident  #00D7FF
    (0, 215, 255),      // Title  (bold)
    (128, 128, 128),    // Dim    #808080
    (78, 78, 78),       // Rule   #4E4E4E
    (158, 214, 114),    // Ok     green
    (255, 175, 0),      // Warn   amber
    (255, 85, 85),      // Crit   red
    (188, 190, 192, ),  // Head   bright grey
];

fn idx(s: St) -> usize {
    match s {
        St::Normal => 0,
        St::Ident => 1,
        St::Title => 2,
        St::Dim => 3,
        St::Rule => 4,
        St::Ok => 5,
        St::Warn => 6,
        St::Crit => 7,
        St::Head => 8,
    }
}

impl St {
    fn color(self) -> Color {
        let (r, g, b) = RGB[idx(self)];
        if r == 0 && g == 0 && b == 0 {
            Color::Reset
        } else {
            Color::Rgb(r, g, b)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Seg {
    pub t: String,
    pub s: St,
}

pub type Row = Vec<Seg>;

/// Single construction point for every display cell. Round-2 finding 21: remote argv,
/// hostnames and kubectl fields are attacker-influenced text that reaches the operator's
/// terminal; control characters here would smuggle in ANSI escapes/extra lines beneath an
/// output path the owner reads as plain text. Strip them where cells are BORN, so no new
/// panel can forget it. (proto::sanitize additionally cleans decoded strings at the wire.)
pub fn seg(t: impl Into<String>, s: St) -> Seg {
    let mut t = t.into();
    if t.chars().any(char::is_control) {
        t.retain(|c| !c.is_control());
    }
    Seg { t, s }
}

pub fn sev_of(pct: f64, warn: f64, crit: f64) -> St {
    if pct >= crit {
        St::Crit
    } else if pct >= warn {
        St::Warn
    } else {
        St::Ok
    }
}

pub fn lamp(st: St) -> &'static str {
    match st {
        St::Ok | St::Normal | St::Dim | St::Rule | St::Head | St::Ident | St::Title => "[o]",
        St::Warn => "[!]",
        St::Crit => "[X]",
    }
}

const CORE_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Segmented horizontal gauge, exactly `width` cells, quarter-capacity tick marks.
/// Cell fill uses the quarter-ramp (`▏▎▌█`) so partial values stay honest.
/// Smooth eighth-block gauge on a transparent background (owner visual pass 2026-10-11:
/// '░' filler + '|' quarter ticks read as noisy scaffolding at table scale; ▁..█ ramps read
/// as one continuous fill). Width is EXACTLY `width` cells — spaces are real cells, so
/// columns stay aligned against {:<14} headers.
pub fn bar(pct: f64, width: usize) -> String {
    const RAMP: [char; 8] = ['\u{250F}', '▏', '▎', '▍', '▌', '▋', '▊', '▉']; // ▏..▉ 1/8..7/8
    let w = width.max(1);
    let p = if pct.is_finite() { pct.clamp(0.0, 100.0) } else { 0.0 };
    let exact = p / 100.0 * w as f64;
    let full = (exact.floor() as usize).min(w);
    let frac = exact - exact.floor();
    // Floor into the eighth-block ramp; a cell only advances once its whole fraction
    // is paid for, so a 5%-of-width cell never looks like a full block.
    let head = if frac <= 0.0 || full >= w {
        None
    } else {
        Some(RAMP[((frac * 8.0).floor() as usize).min(7)])
    };
    let mut out: String = std::iter::repeat('█').take(full).collect();
    if let Some(c) = head {
        out.push(c);
    }
    out.extend(std::iter::repeat(' ').take(w - full - head.map_or(0, |_| 1)));
    out
}

/// Per-core vertical strip: one glyph per core, height ∝ utilization.
pub fn core_strip(values: &[f32], max_cores: usize) -> String {
    let shown = values.len().min(max_cores);
    let mut s = String::new();
    for v in &values[..shown] {
        let v = v.max(0.0) as f64;
        if v <= 0.5 {
            s.push('·');
        } else {
            let i = ((v / 100.0) * 8.0).floor().min(7.0) as usize;
            s.push(CORE_RAMP[i]);
        }
    }
    if values.len() > shown {
        s.push_str(&format!(" +{}", values.len() - shown));
    }
    s
}

/// History sparkline over the absolute 0-100 ramp.
pub fn spark(values: &[f64], width: usize) -> String {
    let start = values.len().saturating_sub(width);
    let mut s = String::new();
    for _ in start..width.saturating_sub(values.len()) {
        s.push(' ');
    }
    for v in &values[start..] {
        let v = v.max(0.0).min(100.0);
        let i = ((v / 100.0) * 8.0).floor().min(7.0) as usize;
        s.push(CORE_RAMP[i]);
    }
    s
}

/// Heavy C2 stencil wordmark (concept sheet Banner B). 14 display columns x 5 rows.
pub const BANNER: [&str; 5] = [
    "██████  ██████",
    "██          ██",
    "██      ██████",
    "██      ██",
    "██████  ██████",
];

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Label of the dedicated fleet-summary tab (owner directive 2026-10-11). app.rs inserts
/// a slot with this name at index 0 in SummaryMode::Overview; render.rs gates chip panels
/// and header text on it. Keep in sync with config's `summary` values.
pub const OVERVIEW_LABEL: &str = "overview";

/// Everything the renderer needs for one frame.
pub struct View<'a> {
    pub clock: String,
    /// Wall-clock Unix seconds at draw time — enables sample-age display (review fix #6).
    pub epoch: i64,
    pub spin: char,
    pub stations: Vec<UiStation<'a>>,
    pub focus: usize,
    /// Where the fleet summary chips belong (config `summary overview|all|off`).
    pub summary: crate::config::SummaryMode,
    pub sort_label: &'static str,
    pub top: usize,
    pub verbose: bool,
    pub paused: bool,
    pub warn: f64,
    pub crit: f64,
    pub width: u16,
    pub height: u16,
}

pub struct UiStation<'a> {
    pub label: String,
    pub host: Option<&'a Station>,
    pub err: Option<String>,
    /// Proxmox cluster view on PVE slots; None on host/k8s slots.
    pub pve: Option<&'a crate::snapshot::PveCluster>,
    pub kube: Option<&'a [KubeNode]>,
    /// Container usage rows and PVC claims from the same fleet poll (empty when unavailable).
    pub kube_pods: &'a [KubePod],
    pub kube_pvcs: &'a [KubePvc],
    /// Human-readable reason `kubectl top` produced no metrics (review fix #23) — kept
    /// verbatim so RBAC/API failures aren't silently labeled "no metrics-server".
    pub kube_note: Option<String>,
    pub hist_cpu: Vec<f64>,
    pub hist_mem: Vec<f64>,
    /// Unix seconds of this slot's last SUCCESSFUL sample (0 = never). Fleet slots have no
    /// Station, hence their own stamp (round-2 finding 9).
    pub sample_ts: i64,
}

impl UiStation<'_> {
    pub fn title(&self) -> String {
        self.label.clone()
    }
}

/// Kubernetes readiness ignores scheduling state: a cordoned node is
/// `Ready,SchedulingDisabled` — still healthy, must not alarm (review fix #23).
pub fn kube_ready(status: &str) -> bool {
    status.split(',').any(|c| c.eq_ignore_ascii_case("Ready"))
}

fn sev_sym(v: f64, view: &View) -> St {
    sev_of(v, view.warn, view.crit)
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn mini_gauge(label: &str, pct: f64, view: &View, width: usize) -> Row {
    let st = sev_sym(pct, view);
    vec![
        seg(format!("{label:<6}"), St::Dim),
        seg("[", St::Rule),
        seg(bar(pct, width), st),
        seg("]", St::Rule),
        seg(format!(" {:>4.0}%", pct), st),
    ]
}

fn header_rows(view: &View) -> Vec<Row> {
    let w = view.width as usize;
    let cur = view.stations.get(view.focus);
    let mut rows: Vec<Row> = Vec::new();

    let station_label = cur.map(|s| s.title()).unwrap_or_default();
    // Fleet slots have no Station/HostInfo, so they previously rendered the same
    // "NO TELEMETRY / station offline" pair as a dead link — while the panel below was
    // demonstrably full of live node rows. Give them their own meta line.
    // The Overview tab has no host of its own; say what it aggregates instead of "NO TELEMETRY".
    let fleet_count = view.stations.iter().filter(|s| s.label != OVERVIEW_LABEL).count();
    let meta = cur
        .filter(|s| s.label != OVERVIEW_LABEL)
        .and_then(|s| s.host)
        .map(|h| format!("UP {} · LOAD {:.1} {:.1} {:.1}", format::fmt_uptime(h.uptime_s), h.load[0], h.load[1], h.load[2]))
        .or_else(|| {
            if cur.is_some_and(|s| s.label == OVERVIEW_LABEL) && fleet_count > 0 {
                return Some(format!(
                    "{fleet_count} STATION{} AGGREGATED",
                    if fleet_count == 1 { "" } else { "S" }
                ));
            }
            cur.and_then(|s| s.kube).map(|k| {
                let ready = k.iter().filter(|n| kube_ready(&n.status)).count();
                format!("FLEET ONLINE · {ready}/{} NODES READY", k.len())
            })
        })
        .or_else(|| {
            cur.and_then(|s| s.pve).map(|c| {
                let online = c.nodes.iter().filter(|n| n.online).count();
                let running = c.guests.iter().filter(|g| g.status == "running").count();
                format!(
                    "PVE CLUSTER · {online}/{} NODES ONLINE · {running}/{} GUESTS RUNNING",
                    c.nodes.len(),
                    c.guests.len()
                )
            })
        })
        .unwrap_or_else(|| "NO TELEMETRY".to_string());
    // Review fix #6: a station with an active error keeps showing last-known data (useful —
    // disk/ps readouts stay valid for minutes) but is stamped ⚠ STALE + sample age so a dead
    // SSH link can never read as nominal.
    let stale_note: Option<String> = match cur {
        Some(s) => match (&s.err, s.host) {
            (Some(e), Some(_)) => Some(format!("⚠ STALE {}", trunc_cell(e, 28))),
            (Some(e), None) => Some(format!("!! {e}")),
            _ => None,
        },
        None => None,
    };
    // Round-2 findings 8/9: age every station slot against its LAST SUCCESSFUL sample —
    // hosts via the frame's ts, fleets via the stamp app.rs records when kubectl succeeds
    // (fleet frames had no timestamp at all, so cached fleets aged invisibly).
    let age_secs: Option<u64> = cur
        .filter(|s| s.sample_ts > 0)
        .map(|s| (view.epoch - s.sample_ts).max(0) as u64)
        .filter(|a| *a >= 5);

    for i in 0..5 {
        let mut r: Vec<Seg> = Vec::new();
        if w >= 58 {
            r.push(seg(format!("{:<14}", BANNER[i]), St::Ident));
        }
        let mid: Vec<Seg> = match i {
            0 => vec![seg("COMMAND CONTROL", St::Title)],
            1 => vec![seg("// SYSTEM COMMAND & CONTROL", St::Dim)],
            2 => {
                // Order matters (round-2 finding 8): append_right_aligned trims LEFT rows
                // from the TAIL, so the operator-critical STALE/AGE cells sit adjacent to the
                // label and the cosmetic uptime/load meta sits last — under width pressure
                // "UP 00d 01:41 LOAD…" is what disappears, never "⚠ STALE ssh: …".
                let mut v = vec![seg(station_label.clone(), St::Ident)];
                if let Some(note) = &stale_note {
                    v.push(seg("  ·  ".to_string(), St::Rule));
                    v.push(seg(note.clone(), St::Warn));
                }
                if let Some(a) = age_secs {
                    v.push(seg(format!("  ·  AGE {a}s"), St::Warn));
                }
                v.push(seg("  ·  ".to_string(), St::Rule));
                v.push(seg(meta.clone(), St::Dim));
                v
            }
            3 => cur
                .and_then(|s| s.host)
                .map(|h| vec![seg(trunc_cell(&h.os_name, 28), St::Dim), seg(" · kernel ", St::Rule), seg(trunc_cell(&h.kernel, 22), St::Dim)])
                // Fleet slots report their data source where a host row shows OS/kernel;
                // "station offline" is reserved for slots with neither.
                .unwrap_or_else(|| {
                    if cur.is_some_and(|s| s.label == OVERVIEW_LABEL) {
                        return vec![seg(
                            format!("all stations · TAB/1..{} to drill in", fleet_count),
                            St::Dim,
                        )];
                    }
                    if let Some(k) = cur.and_then(|s| s.kube) {
                        vec![
                            seg("kubectl fleet", St::Dim),
                            seg(" · ", St::Rule),
                            seg(format!("{} node{} via kubeconfig", k.len(), if k.len() == 1 { "" } else { "s" }), St::Dim),
                        ]
                    } else if let Some(c) = cur.and_then(|s| s.pve) {
                        vec![
                            seg("proxmox cluster", St::Dim),
                            seg(" · ", St::Rule),
                            seg(
                                format!("{} hypervisor{} via pvesh", c.nodes.len(), if c.nodes.len() == 1 { "" } else { "s" }),
                                St::Dim,
                            ),
                        ]
                    } else {
                        vec![seg("station offline".to_string(), St::Warn)]
                    }
                }),
            4 => vec![seg(view.clock.clone(), St::Dim), seg("  ", St::Normal), seg(format!("[{}] SAMPLE", view.spin), St::Ident)],
            _ => vec![],
        };
        r.extend(mid);
        rows.push(r);
    }
    // Right rail gauges for the focused station (rows 1..4). Width-tiered (owner report
    // 2026-10-11: fleet tab showed ZERO gauges on wide-looking real terminals — a fixed
    // gate silently suppresses ALL visuals when effective columns fall below it, exactly
    // what HiDPI/large-font terminals report). Tiers degrade the RAIL, never remove it:
    // rich >= 92 cols, compact >= 40, numeric-text >= 24.
    if let Some(s) = cur {
        let host_rails: Vec<(&str, f64)> = match s.host {
            Some(h) => {
                let nvram = gpu_avg_pct(h);
                vec![("CPU", h.cpu_avg_pct as f64), ("MEM", h.mem_pct()), ("NVRAM", nvram), ("DISK", disk_max_pct(h))]
            }
            None => vec![("CPU", 0.0), ("MEM", 0.0), ("NVRAM", 0.0), ("DISK", 0.0)],
        };
        // Fleet slots roll up `kubectl top` usage-vs-allocatable; nodes without a metric are
        // skipped so metrics-server gaps can't read as 0%. NVRAM/DISK are concepts a fleet
        // does not have — showing 0% there is noise (and reads as "all nodes at zero").
        // PVE slots use the same two-rail vocabulary from hypervisor data: CPU = mean of
        // ONLINE nodes' cpu %; MEM = total used bytes over total capacity (offline nodes
        // contribute nothing — counting their RAM as free would lie both ways).
        let mut rails: Vec<(&str, f64)> = match (s.kube, s.pve) {
            (Some(k), _) => {
                let avg = |get: fn(&KubeNode) -> Option<f32>| -> f64 {
                    let v: Vec<f64> = k.iter().filter_map(get).map(f64::from).collect();
                    if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
                };
                vec![("CPU", avg(|n| n.cpu_pct)), ("MEM", avg(|n| n.mem_pct))]
            }
            (None, Some(c)) => {
                let online: Vec<&crate::snapshot::PveNode> = c.nodes.iter().filter(|n| n.online).collect();
                let cpus: Vec<f64> = online.iter().filter_map(|n| n.cpu_pct).collect();
                let cpu_avg = if cpus.is_empty() { 0.0 } else { cpus.iter().sum::<f64>() / cpus.len() as f64 };
                let used: u64 = online.iter().map(|n| n.mem_bytes).sum();
                let total: u64 = online.iter().map(|n| n.max_mem_bytes).sum();
                let mem = crate::snapshot::pct_of(used, total);
                vec![("CPU", cpu_avg), ("MEM", mem)]
            }
            (None, None) => host_rails.clone(),
        };
        // Full bars at >= 40 columns; pure numbers under 40 (bar+brackets+label need ~21
        // cells to stay readable and not eat the title). Below 24 nothing fits at all.
        let bar_mode = w >= 40;
        if !bar_mode {
            rails.retain(|(label, _)| matches!(*label, "CPU" | "MEM"));
        }
        for (i, (label, pct)) in rails.iter().enumerate().take(4) {
            let row_idx = (1 + i).min(rows.len() - 1);
            let gauge: Row = if bar_mode {
                let mut r = mini_gauge(label, *pct, view, 8);
                r.push(seg(" ".to_string(), St::Normal));
                r
            } else {
                let st = sev_sym(*pct, view);
                vec![seg(format!("{label:>3} {:.0}%", *pct), st)]
            };
            append_right_aligned(&mut rows[row_idx], &gauge, w);
        }
        if let Some(k) = s.kube {
            let ready = k.iter().filter(|n| kube_ready(&n.status)).count();
            append_right_aligned(
                &mut rows[0],
                &[seg(format!("NODES {ready}/{}", k.len()), St::Ident)],
                w,
            );
        }
        if let Some(c) = s.pve {
            let online = c.nodes.iter().filter(|n| n.online).count();
            append_right_aligned(
                &mut rows[0],
                &[seg(format!("PVE {online}/{}", c.nodes.len()), St::Ident)],
                w,
            );
        }
    }
    rows
}

/// Truncate to at most `max_w` DISPLAY CELLS (not chars), reserving the ellipsis only when
/// truncation actually happens. Review fix #18: the old version appended '…' even for exact
/// fits (`trunc_cell("abc", 3)` returned "ab…") and produced output for width 0.
pub fn trunc_cell(s: &str, max_w: usize) -> String {
    // Round-2 finding 27: with a zero-cell budget the ellipsis itself would not fit —
    // the old loop emitted '…' (width 1) into a width-0 field, shifting every later column.
    if max_w == 0 {
        return String::new();
    }
    if visible_width(s) <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        // An ellipsis costs one cell; stop before the budget is crossed.
        if width + cw >= max_w {
            break;
        }
        out.push(c);
        width += cw;
    }
    out.push('…');
    out
}

fn gpu_avg_pct(h: &Station) -> f64 {
    if h.gpus.is_empty() {
        0.0
    } else {
        h.gpus.iter().map(|g| g.vram_pct()).sum::<f64>() / h.gpus.len() as f64
    }
}

pub fn disk_max_pct(h: &Station) -> f64 {
    h.disks.iter().map(|d| d.pct()).fold(0.0f64, f64::max)
}

/// Pad/truncate every row to the terminal width; push right-aligned content flush.
/// Review fix #25: right-rail gauges own their columns — overflowing left content is trimmed
/// (segment-by-segment from the right) so a long kernel string can never collide with them.
fn append_right_aligned(row: &mut Row, rhs: &[Seg], width: usize) {
    let right: usize = rhs.iter().map(|s| visible_width(&s.t)).sum();
    let mut left: usize = row.iter().map(|s| visible_width(&s.t)).sum();
    while left + right + 1 > width {
        match row.pop() {
            Some(sg) => left -= visible_width(&sg.t),
            None => break,
        }
    }
    let gap = width.saturating_sub(left + right);
    row.push(seg(" ".repeat(gap), St::Dim));
    row.extend(rhs.iter().cloned());
}

pub fn visible_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn row_width(row: &[Seg]) -> usize {
    row.iter().map(|s| visible_width(&s.t)).sum()
}

/// Pad/truncate to EXACTLY `width` cells. Review fix #18: truncation must not abandon the
/// remainder of the row (a wide CJK glyph at the boundary used to drop everything after it)
/// and must still pad to the full budget, so table columns stay aligned under clipping.
fn finish_row(mut row: Row, width: usize) -> Row {
    let used = row_width(&row);
    if used > width {
        let mut acc = 0;
        let mut out: Row = Vec::new();
        'outer: for sg in row.drain(..) {
            for c in sg.t.chars() {
                let cw = c.width().unwrap_or(0);
                if acc + cw > width {
                    continue; // skip over-sized glyphs, keep scanning later content
                }
                out.push(seg(c.to_string(), sg.s));
                acc += cw;
                if acc == width {
                    break 'outer;
                }
            }
        }
        // A skipped wide char can leave slack; fill it so width is exact.
        if acc < width {
            out.push(seg(" ".repeat(width - acc), St::Normal));
        }
        return out;
    }
    if used < width {
        row.push(seg(" ".repeat(width - used), St::Normal));
    }
    row
}

fn rule(w: usize) -> Row {
    vec![seg("─".repeat(w), St::Rule)]
}

fn section_title(name: &str, extra: Row, w: usize) -> Row {
    let mut r = vec![seg(format!("{name:<5}"), St::Head)];
    r.extend(extra);
    let _ = w;
    r
}

fn cpu_panel(view: &View, h: &Station, hist: &[f64]) -> Vec<Row> {
    let w = view.width as usize;
    let lampst = sev_of(h.cpu_avg_pct as f64, view.warn, view.crit);
    let mut rows = Vec::new();
    rows.push(section_title(
        "CPU",
        vec![
            seg(format!("{} cores", h.core_count), St::Dim),
            seg(" · util ", St::Rule),
            seg(format!("{:.1}%", h.cpu_avg_pct), lampst),
            seg(" ", St::Normal),
            seg(lamp(lampst), lampst),
        ],
        w,
    ));
    let cap = w.saturating_sub(12).max(4);
    rows.push(vec![
        seg("CORE  ", St::Dim),
        seg(core_strip(&h.per_core_pct, cap), lampst),
    ]);
    rows.push(vec![
        seg("HIST  ", St::Dim),
        seg(spark(hist, cap), St::Ident),
    ]);
    rows.push(vec![
        seg("UTIL  ", St::Dim),
        seg("[", St::Rule),
        seg(bar(h.cpu_avg_pct as f64, (w / 3).max(10)), lampst),
        seg("]", St::Rule),
    ]);
    rows
}

/// History now arrives with the focused slot itself — review fix #20: selecting by display
/// label vs reported hostname produced blank HIST panels for `--station PROD` and custom
/// ssh labels, and duplicate labels could crosswire stations.
fn mem_panel(view: &View, h: &Station, hist: &[f64]) -> Vec<Row> {
    let w = view.width as usize;
    let st = sev_of(h.mem_pct(), view.warn, view.crit);
    let mut rows = vec![vec![
        seg("MEM ", St::Head),
        seg(
            format!(
                "{}/{} used · avail {}",
                format::fmt_bytes(h.mem_used_kb() * 1024),
                format::fmt_bytes(h.mem_total_kb * 1024),
                format::fmt_bytes(h.mem_avail_kb * 1024)
            ),
            St::Dim,
        ),
    ]];
    rows.push(vec![
        seg("USE ", St::Dim),
        seg("[", St::Rule),
        seg(bar(h.mem_pct(), (w / 3).max(10)), st),
        seg("]", St::Rule),
        seg(format!(" {:>5.1}% ", h.mem_pct()), st),
        seg(lamp(st), st),
    ]);
    if h.swap_total_kb > 0 {
        rows.push(vec![
            seg("SWAP", St::Dim),
            seg("[", St::Rule),
            seg(bar(h.swap_pct(), (w / 3).max(10)), sev_of(h.swap_pct(), view.warn, view.crit)),
            seg("]", St::Rule),
            seg(format!(" {:>5.1}%", h.swap_pct()), St::Dim),
        ]);
    }
    rows.push(vec![
        seg("HIST", St::Dim),
        seg(spark(hist, (w / 3).max(10).min(48)), St::Ident),
    ]);
    rows
}

fn nvram_panel(view: &View, h: &Station) -> Vec<Row> {
    let w = view.width as usize;
    let mut rows = Vec::new();
    if h.gpus.is_empty() {
        rows.push(vec![
            seg("NVRAM", St::Head),
            seg("  [ ] no GPU telemetry (nvidia-smi / amdgpu sysfs not found)", St::Dim),
        ]);
        return rows;
    }
    for g in &h.gpus {
        let st = sev_of(g.vram_pct(), view.warn, view.crit);
        // nvidia-smi/amdgpu names usually carry the vendor word already.
        let vendor = match g.vendor {
            crate::snapshot::GpuVendor::Nvidia if !g.name.to_ascii_uppercase().contains("NVIDIA") => "NVIDIA ",
            crate::snapshot::GpuVendor::Amd if !g.name.to_ascii_uppercase().starts_with("AMD") => "AMD ",
            _ => "",
        };
        let temp = g.temp_c.map(|t| format!(" · {t:.0}°C")).unwrap_or_default();
        rows.push(vec![
            seg("NVRAM", St::Head),
            seg(format!(" {vendor}{} · util {:.0}%{temp}", trunc_cell(&g.name, 34 - vendor.len()), g.util_pct), St::Dim),
        ]);
        // Integrated/unified parts (GB10): capacity may be knowable from host RAM while
        // usage comes from CUDA process totals — and vice versa. Draw what we have.
        match (g.vram_used_mib, g.vram_total_mib) {
            (Some(used), Some(total)) if total > 0.0 => rows.push(vec![
                seg("VRAM ", St::Dim),
                seg("[", St::Rule),
                seg(bar(g.vram_pct(), (w / 3).max(10)), st),
                seg("]", St::Rule),
                seg(
                    format!(" {:.1}/{:.1} GiB ({:.0}%) ", used / 1024.0, total / 1024.0, g.vram_pct()),
                    st,
                ),
                seg(lamp(st), st),
            ]),
            (u, Some(total)) if total > 0.0 => rows.push(vec![
                seg("VRAM ", St::Dim),
                seg(
                    format!(
                        " unified pool {:.1} GiB{}",
                        total / 1024.0,
                        u.map(|_| "").unwrap_or(" · per-process usage unreadable")
                    ),
                    St::Dim,
                ),
            ]),
            _ => rows.push(vec![
                seg("VRAM ", St::Dim),
                seg("utilization-only device · unified memory, capacity not exposed", St::Dim),
            ]),
        }
    }
    rows
}

fn net_panel(_view: &View, h: &Station, iface_cap: usize) -> Vec<Row> {
    let total_rx: f64 = h.net.iter().map(|n| n.rx_bps).sum();
    let total_tx: f64 = h.net.iter().map(|n| n.tx_bps).sum();
    let mut rows = vec![vec![
        seg("NET ", St::Head),
        seg(format!("▼ {}", format::fmt_rate(total_rx)), St::Ok),
        seg("  ▲ ", St::Rule),
        seg(format::fmt_rate(total_tx), St::Warn),
        seg("  (last interval)", St::Dim),
    ]];
    let mut sorted = h.net.clone();
    sorted.sort_by(|a, b| (b.rx_bps + b.tx_bps).total_cmp(&(a.rx_bps + a.tx_bps)));
    for n in sorted.into_iter().take(iface_cap) {
        let link = n.link_speed_mbps.map(|s| format!(" · {s} Mb/s link")).unwrap_or_default();
        // fmt_rate() already ends in "/s"; the old "{:>10}/s" printed "MiB/s/s". (Review fix #25.)
        rows.push(vec![
            seg("IFACE ", St::Dim),
            seg(format!("{:<10}", trunc_cell(&n.iface, 10)), St::Normal),
            seg("▼ ", St::Ok),
            seg(rate_cell(n.rx_bps), St::Normal),
            seg("  ▲ ", St::Rule),
            seg(rate_cell(n.tx_bps), St::Normal),
            seg(link, St::Dim),
        ]);
    }
    rows
}

/// One rate in a FIXED-width cell so IFACE columns stay aligned (round-2 finding 26:
/// `{:>9}` only holds while the text fits 9 cells; "1023.9 MiB/s" is 12 and shifted
/// every later column). 14 covers fmt_bytes' widest honest string ("1023.9 PiB/s");
/// anything beyond truncates rather than desynchronizing the table.
fn rate_cell(bytes_per_sec: f64) -> String {
    let s = format::fmt_rate(bytes_per_sec);
    if visible_width(&s) <= 14 {
        format!("{s:>14}")
    } else {
        trunc_cell(&s, 14)
    }
}

fn disk_panel(view: &View, h: &Station, max_rows: usize) -> Vec<Row> {
    let w = view.width as usize;
    let mut rows = Vec::new();
    for d in h.disks.iter().take(max_rows) {
        let st = sev_of(d.pct(), view.warn, view.crit);
        rows.push(vec![
            seg("DISK ", St::Head),
            seg(format!("{:<14}", trunc_cell(&d.mount, 14)), St::Normal),
            seg(format!("{:<6}", trunc_cell(&d.fstype, 6)), St::Dim),
            seg("[", St::Rule),
            seg(bar(d.pct(), (w / 4).max(8)), st),
            seg("]", St::Rule),
            seg(format!(" {:>5.1}% ", d.pct()), st),
            seg(
                format!("{}/{}", format::fmt_bytes(d.used_bytes()), format::fmt_bytes(d.total_bytes)),
                St::Dim,
            ),
            seg(lamp(st), st),
        ]);
    }
    rows
}

/// Compact per-station strip. `with_disk` adds a disk-max cell (the Overview tab is the
/// one place all "important stats" belong per owner 2026-10-11). The overview slot itself
/// never gets a chip — it has no telemetry of its own, only the fleet's.
fn fleet_chips(view: &View, with_disk: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for (i, s) in view.stations.iter().enumerate() {
        if s.kube.is_some() || s.label == OVERVIEW_LABEL {
            continue; // k8s rows belong to kube_table; the overview tab has no telemetry of its own
        }
        let (lamp_st, tail) = match (s.host, &s.err) {
            // Stale-but-readable station: keep the numbers, stamp the fault (#6).
            (Some(h), Some(e)) => {
                let nv = gpu_avg_pct(h);
                (
                    St::Crit,
                    format!(
                        "STALE·{}  cpu {:>4.0}%  mem {:>4.0}%  nvram {:>4.0}%",
                        trunc_cell(e, 18),
                        h.cpu_avg_pct,
                        h.mem_pct(),
                        nv
                    ),
                )
            }
            (Some(h), None) => {
                let l = sev_of((h.cpu_avg_pct as f64).max(h.mem_pct()), view.warn, view.crit);
                let nv = gpu_avg_pct(h);
                let mut tail = format!(
                    "cpu {:>4.0}%  mem {:>4.0}%  nvram {:>4.0}%  load {:.1}",
                    h.cpu_avg_pct, h.mem_pct(), nv, h.load[0]
                );
                if with_disk {
                    // Overview tab is the consolidated dashboard owner asked for: one
                    // glance needs disk too, not just cpu/mem/nvram/load.
                    tail.push_str(&format!("  disk {:>4.0}%", disk_max_pct(h)));
                }
                (l, tail)
            }
            (None, Some(e)) => (St::Crit, trunc_cell(e, 40)),
            (None, None) => match s.pve {
                // A live PVE slot has no Station host — say what it IS instead of the
                // misleading "awaiting first sample…" a bare arm would print (owner saw
                // "[9] PVE:lab awaiting" while the cluster tab streamed data).
                Some(c) => {
                    let online = c.nodes.iter().filter(|n| n.online).count();
                    let running = c.guests.iter().filter(|g| g.status == "running").count();
                    (
                        if online == c.nodes.len() { St::Ok } else { St::Warn },
                        format!(
                            "{online}/{} hypervisor{} online · {running}/{} guests",
                            c.nodes.len(),
                            if c.nodes.len() == 1 { "" } else { "s" },
                            c.guests.len()
                        ),
                    )
                }
                None => (St::Dim, "awaiting first sample…".to_string()),
            },
        };
        // No [o]/[X] lamp here (owner, 2026-10-11): the tab strip and tail already carry
        // status through color; bracketed letters added noise to every row. Rail gauges keep theirs.
        rows.push(vec![
            seg(format!("[{}] ", i + 1), St::Dim),
            seg(format!("{:<14}", trunc_cell(&s.title(), 14)), if i == view.focus { St::Ident } else { St::Normal }),
            seg(tail, lamp_st),
        ]);
    }
    rows
}

/// Proxmox cluster panel: hypervisor nodes, guest VM/CT inventory, storage usage and the
/// Ceph health summary. Focused-slot only (same rule as kube_table — the fedora tab must
/// not paint proxmox tables above its local gauges).
fn pve_table(view: &View) -> Vec<Row> {
    let mut rows = Vec::new();
    let Some(st) = view.stations.get(view.focus.min(view.stations.len().saturating_sub(1))) else {
        return rows;
    };
    let Some(cluster) = st.pve else { return rows };

    // ---- PVE NODES: hypervisor hardware, inline heat bars like the k8s table ----
    if !cluster.nodes.is_empty() {
        rows.push(vec![seg(
            format!("{:<14} {:<7} {:<15} {:<14} {:>6} {:<14}", "PVE NODE", "STATUS", "IP", "CPU", "CORES", "MEM"),
            St::Head,
        )]);
        // A 40-node cluster would eat the screen: running capacity first is more useful
        // than alphabetical — online nodes sorted by CPU load descending.
        let mut nodes: Vec<&crate::snapshot::PveNode> = cluster.nodes.iter().collect();
        nodes.sort_by(|a, b| {
            b.online.cmp(&a.online).then_with(|| b.cpu_pct.unwrap_or(0.0).total_cmp(&a.cpu_pct.unwrap_or(0.0)))
        });
        for n in nodes.into_iter().take(if view.verbose { 64 } else { 12 }) {
            let stt = if n.online { St::Ok } else { St::Crit };
            // One 14-cell metric column: bar(9) + right-aligned percent, same contract as
            // kube_table's cells ({:>3}% keeps 1..=100 aligned against {:<14} headers).
            let cell = |p: Option<f64>| -> (String, St) {
                match p {
                    Some(v) => (
                        format!("{:<14}", format!("{} {:>3}%", bar(v, 9), v.round())),
                        sev_sym(v, view),
                    ),
                    None => (format!("{:<14}", "-"), St::Dim),
                }
            };
            let (cpu_s, cpu_c) = cell(n.cpu_pct);
            // Node memory % is computed from bytes here (pvesh gives GiB usage/total).
            let mem_pct = if n.max_mem_bytes > 0 {
                Some(100.0 * n.mem_bytes as f64 / n.max_mem_bytes as f64)
            } else {
                None
            };
            let (mem_s, mem_c) = cell(mem_pct);
            rows.push(vec![
                seg(format!("{:<14} ", trunc_cell(&n.name, 14)), St::Normal),
                seg(format!("{:<7} ", trunc_cell(if n.online { "online" } else { "OFFLINE" }, 7)), stt),
                seg(format!("{:<15} ", trunc_cell(&n.ip, 15)), St::Dim),
                seg(format!("{cpu_s} "), cpu_c),
                seg(format!("{:>6} ", if n.max_cpu > 0 { format!("{}c", n.max_cpu) } else { "-".into() }), St::Dim),
                seg(mem_s, mem_c),
                seg(format!(" up {}", format::fmt_uptime(n.uptime_s)), St::Dim),
            ]);
        }
    }

    // ---- GUESTS: VMs and containers, ranked like TOP PROCS ----
    if !cluster.guests.is_empty() {
        rows.push(vec![seg(String::new(), St::Dim)]);
        let mut guests: Vec<&crate::snapshot::PveGuest> = cluster.guests.iter().collect();
        // Running first, then hottest CPU: a stopped template never outranks a busy VM.
        guests.sort_by(|a, b| {
            let ra = if a.status == "running" { 1u8 } else { 0 };
            let rb = if b.status == "running" { 1u8 } else { 0 };
            rb.cmp(&ra).then_with(|| {
                b.cpu_pct.unwrap_or(0.0).total_cmp(&a.cpu_pct.unwrap_or(0.0))
            })
        });
        let running = cluster.guests.iter().filter(|g| g.status == "running").count();
        rows.push(vec![seg(
            format!(
                "GUESTS · {} of {} running (bar: % of own cores)",
                running,
                cluster.guests.len()
            ),
            St::Head,
        )]);
        rows.push(vec![seg(
            format!("{:>4} {:<5} {:<24} {:<7} {:<12} {:>4} {:>9}", "VMID", "TYPE", "NAME", "NODE", "CPU", "CORES", "MEM"),
            St::Head,
        )]);
        let cap = if view.verbose { 32 } else { 10 };
        let max_share = guests.iter().filter_map(|g| g.cpu_pct).fold(0.0f64, f64::max);
        for g in guests.into_iter().take(cap) {
            let stat = match g.status.as_str() {
                "running" => St::Ok,
                "stopped" => St::Dim,
                "" => St::Dim,
                _ => St::Warn, // paused, migrating, backup-suspended… worth attention
            };
            let share = g.cpu_pct.map(|p| (p / max_share.max(f64::EPSILON) * 100.0).clamp(1.0, 100.0));
            let bar_s = match share {
                Some(s) => seg(bar(s, 10), sev_sym(s, view)),
                None => seg(" ".repeat(10), St::Dim),
            };
            rows.push(vec![
                seg(format!("{:>4} ", g.vmid), St::Normal),
                seg(format!("{:<5} ", if g.kind == "lxc" { "lxc" } else { "qemu" }), St::Dim),
                seg(
                    format!("{:<24} ", format!("{}{}", trunc_cell(&g.name, 23 - usize::from(g.template)), if g.template { " T" } else { "" })),
                    if g.template { St::Dim } else { St::Normal },
                ),
                seg(format!("{:<7} ", trunc_cell(&g.node, 7)), St::Dim),
                bar_s,
                seg(
                    format!(" {:>3} ", if g.max_cpu > 0 { g.max_cpu.to_string() } else { "-".into() }),
                    St::Dim,
                ),
                seg(
                    format!("{:>9}", format::fmt_bytes(g.mem_bytes)),
                    St::Dim,
                ),
                seg(format!(" {:<7}", trunc_cell(&g.status, 7)), stat),
            ]);
        }
    }

    // ---- STORAGE: NFS, dirs, LVM-thin, CephFS/RBD — every storage the nodes mount ----
    if !cluster.storage.is_empty() {
        rows.push(vec![seg(String::new(), St::Dim)]);
        let mut stor: Vec<&crate::snapshot::PveStorage> = cluster.storage.iter().collect();
        // Hottest volume first: the operator cares about the pool that is filling up.
        stor.sort_by(|a, b| b.used_pct.unwrap_or(-1.0).total_cmp(&a.used_pct.unwrap_or(-1.0)));
        rows.push(vec![seg(
            format!("STORAGE · {} volumes · bar: % full (per-node view)", stor.len()),
            St::Head,
        )]);
        rows.push(vec![seg(
            format!("{:<7} {:<14} {:<8} {:<12} {:>6} {:>10} {:>10}", "NODE", "NAME", "TYPE", "USE", "%FULL", "USED", "AVAIL"),
            St::Head,
        )]);
        // Capacity pin: a huge low-percentage volume (the 1.26 TiB CephFS sits at ~3.8% full)
        // falls off a hottest-first cut, and the owner explicitly wants cephfs/NFS usage
        // visible — so append the two largest volumes that didn't make the heat cut.
        let heat_cap = if view.verbose { 40 } else { 10 };
        let pinned = if view.verbose { 0 } else { 2 };
        let mut shown: Vec<&crate::snapshot::PveStorage> = stor.iter().copied().take(heat_cap).collect();
        let mut rest = stor.split_off(heat_cap.min(stor.len()));
        rest.sort_by_key(|s| std::cmp::Reverse(s.total_bytes));
        shown.extend(rest.into_iter().take(pinned));
        for s in shown {
            let stt = match s.used_pct {
                Some(p) => sev_sym(p, view),
                None => St::Dim,
            };
            rows.push(vec![
                seg(format!("{:<7} ", trunc_cell(&s.node, 7)), St::Dim),
                seg(format!("{:<14} ", trunc_cell(&s.name, 14)), St::Normal),
                seg(format!("{:<8} ", trunc_cell(&s.kind, 8)), St::Dim),
                match s.used_pct {
                    Some(p) => seg(bar(p, 10), stt),
                    None => seg("-".repeat(10), St::Dim),
                },
                seg(
                    format!(" {:>5}%", s.used_pct.map(|p| p.round()).unwrap_or_default()),
                    stt,
                ),
                seg(format!(" {:>9}", format::fmt_bytes(s.used_bytes)), St::Dim),
                seg(format!(" {:>9}", format::fmt_bytes(s.avail_bytes)), St::Dim),
            ]);
        }
    }

    // ---- Ceph health: one line, colored honestly ----
    if let Some(h) = &cluster.ceph_health {
        let st = ceph_st(h);
        rows.push(vec![
            seg("CEPH  ", St::Head),
            seg(format!("health {h}"), st),
        ]);
    }

    // Collection notes (no pvesh, API failures…) — small, dim, capped.
    for note in cluster.notes.iter().take(if view.verbose { 8 } else { 3 }) {
        rows.push(vec![seg(format!("       {}", trunc_cell(note, 90)), St::Dim)]);
    }
    rows
}

/// Map a Ceph overall_status string onto lamp colors: anything not OK-ish is a problem.
pub fn ceph_st(status: &str) -> St {
    match status.trim().to_ascii_uppercase().as_str() {
        "OK" | "HEALTH_OK" => St::Ok,
        "WARN" | "HEALTH_WARN" | "WARNING" => St::Warn,
        _ => St::Crit, // HEALTH_ERR, AVAILABILITY, unavailable strings…
    }
}

/// Machine-readable PVE alerts (offline hypervisors, full storage volumes, sick Ceph)
/// shared by alert_lines and alert_count so they can never disagree (round-2 pattern).
fn pve_alerts(view: &View, warn: f64) -> Vec<(St, String)> {
    let mut out = Vec::new();
    for s in &view.stations {
        let Some(c) = s.pve else { continue };
        for n in c.nodes.iter().filter(|n| !n.online) {
            out.push((St::Crit, format!("PVE NODE {} OFFLINE", n.name)));
        }
        for g in c.guests.iter().filter(|g| g.status == "paused") {
            out.push((St::Warn, format!("GUEST {} {} PAUSED", g.vmid, g.name)));
        }
        for st in &c.storage {
            // Only judge volumes we can measure; skip tiny sub-GiB scratch mounts.
            if st.total_bytes >= 1 << 30 {
                if let Some(p) = st.used_pct {
                    if p >= warn {
                        out.push((
                            sev_sym(p, view),
                            format!("STORAGE {}/{} {:.0}% FULL ({} free)", st.node, st.name, p, format::fmt_bytes(st.avail_bytes)),
                        ));
                    }
                }
            }
        }
        if let Some(h) = &c.ceph_health {
            let st = ceph_st(h);
            if st != St::Ok {
                out.push((st, format!("CEPH HEALTH {h}")));
            }
        }
    }
    out
}

fn kube_table(view: &View) -> Vec<Row> {
    let mut rows = Vec::new();
    // Owner report 2026-10-11: the fleet panel leaked onto EVERY tab — the old lookup took
    // the FIRST station carrying kube data, so opening the `fedora` tab rendered the K8S
    // node table, TOP PODS and PVC inventory above the local gauges. Panels belong to the
    // FOCUSED station; a host slot has `kube: None` and gets none of them.
    let Some(st) = view.stations.get(view.focus.min(view.stations.len().saturating_sub(1))) else {
        return rows;
    };
    let Some(nodes) = st.kube else { return rows };
    // Owner report 2026-10-11: "k8s shows no visuals or graphs". Node CPU/MEM columns
    // carry an inline heat bar + number per row, colored against --warn/--crit. Cells widen
    // from {:>6} to {:<14}; narrow windows lose the tail via finish_row like every table here.
    rows.push(vec![
        seg("FLEET ", St::Head),
        seg(format!("{:<22} {:<12} {:<14} {:<14} {:<14}", "NODE", "STATUS", "ROLES", "CPU", "MEM"), St::Head),
    ]);
    for n in nodes.iter().take(if view.verbose { 32 } else { 8 }) {
        // Review fix #23: readiness is one of the comma-joined conditions — a cordoned
        // "Ready,SchedulingDisabled" node is healthy, not an alarm.
        let st = if kube_ready(&n.status) { St::Ok } else { St::Crit };
        // Field separators must mirror the header format EXACTLY (round-2 era layout bug
        // caught in live k8s capture: header has spaces between columns, rows had none, so
        // a 22-char hostname rendered flush against "Ready").
        // Fixed 14-cell column: bar(9) + space + right-aligned percent ({:>4} keeps the
        // cell exact for 1..=100, so roles/CPU/MEM stay aligned against their {:<14} headers.
        let cell = |p: Option<f32>| -> (String, St) {
            match p {
                Some(v) => (format!("{:<14}", format!("{} {:>3}%", bar(f64::from(v).clamp(0.0, 100.0), 9), v.round())), sev_sym(f64::from(v), view)),
                None => (format!("{:<14}", "-"), St::Dim),
            }
        };
        let (cpu_s, cpu_c) = cell(n.cpu_pct);
        let (mem_s, mem_c) = cell(n.mem_pct);
        rows.push(vec![
            seg(format!("{:<22} ", trunc_cell(&n.name, 22)), St::Normal),
            // Round-2 finding 32: "Ready,SchedulingDisabled" is 22 cells — the header says
            // STATUS at 12; truncate to keep roles/metrics columns aligned.
            seg(format!("{:<12} ", trunc_cell(&n.status, 12)), st),
            seg(format!("{:<14} ", trunc_cell(&n.roles, 14)), St::Dim),
            seg(format!("{cpu_s} "), cpu_c),
            seg(mem_s, mem_c),
        ]);
    }
    if !nodes.is_empty() && nodes[0].cpu_millis.is_none() && nodes[0].cpu_pct.is_none() {
        // Review fix #23: show kubectl's actual failure (RBAC, API timeout…) when we have it.
        let why = st
            .kube_note
            .as_deref()
            .map(|e| trunc_cell(e, 76))
            .unwrap_or_else(|| "metrics-server reports no metrics — showing node status only".to_string());
        rows.push(vec![seg(format!("       {why}"), St::Dim)]);
    }

    // TOP PODS — the cluster-native analog of TOP PROCS. Kubernetes exposes containers,
    // not OS processes (kubectl cannot see host PIDs — that needs an SSH station on each
    // node). Bars are RELATIVE to the hottest container in this poll: a heat map for
    // "what is hot in my cluster", not an absolute-% reading.
    if !st.kube_pods.is_empty() {
        rows.push(vec![seg(String::new(), St::Dim)]); // section breathing room (visual pass)
        let mut ranked: Vec<&crate::snapshot::KubePod> = st.kube_pods.iter().collect();
        ranked.sort_by(|a, b| b.cpu_milli.cmp(&a.cpu_milli).then_with(|| b.mem_bytes.cmp(&a.mem_bytes)));
        let n_total = ranked.len();
        let cap = if view.verbose { 16 } else { 8 };
        let max_cpu = ranked.first().map(|p| p.cpu_milli.max(1)).unwrap_or(1);
        rows.push(vec![seg(format!("TOP PODS · usage via kubectl top · {} of {n_total} shown (bar: % of hottest)", cap.min(n_total)), St::Head)]);
        rows.push(vec![seg(format!("{:<12} {:<22} {:<10} {:<10} {:>5} {:>8}", "NS", "POD", "CONTAINER", "CPU", "CORES", "MEM"), St::Head)]);
        for p in ranked.into_iter().take(cap) {
            let share = ((p.cpu_milli as f64 / max_cpu as f64) * 100.0).clamp(1.0, 100.0);
            rows.push(vec![
                seg(format!("{:<12} ", trunc_cell(&p.namespace, 12)), St::Dim),
                seg(format!("{:<22} ", trunc_cell(&p.pod, 22)), St::Normal),
                seg(format!("{:<10} ", trunc_cell(&p.container, 10)), St::Dim),
                seg(bar(share, 10), sev_sym(share, view)),
                seg(format!(" {:>4} ", cpu_label(p.cpu_milli)), St::Normal),
                seg(format!("{:>8}", format::fmt_bytes(p.mem_bytes)), St::Dim),
            ]);
        }
    }
    if st.kube_pods.is_empty() {
        if let Some(why) = split_note(st.kube_note.as_deref(), "pods") {
            rows.push(vec![seg(format!("       pod metrics unavailable: {}", trunc_cell(&why, 76)), St::Dim)]);
        }
    }

    // PVC — provisioned storage claims. Metrics-server does not report filesystem USAGE
    // inside a volume, so ccc shows capacity/status/class truthfully instead of inventing
    // a percentage; true usage needs Prometheus/kubelet stats.
    if !st.kube_pvcs.is_empty() {
        rows.push(vec![seg(String::new(), St::Dim)]);
        let mut ranked: Vec<&crate::snapshot::KubePvc> = st.kube_pvcs.iter().collect();
        ranked.sort_by_key(|p| std::cmp::Reverse(p.capacity_bytes.unwrap_or(0)));
        let bound = ranked.iter().filter(|p| p.phase == "Bound").count();
        let provisioned = format::fmt_bytes(ranked.iter().filter_map(|p| p.capacity_bytes).sum::<u64>());
        rows.push(vec![seg(format!("PVC · {bound}/{} bound · {provisioned} provisioned", ranked.len()), St::Head)]);
        rows.push(vec![seg(format!("{:<12} {:<22} {:<9} {:>9} {:<14}", "NS", "CLAIM", "PHASE", "SIZE", "CLASS"), St::Head)]);
        for c in ranked.into_iter().take(if view.verbose { 16 } else { 6 }) {
            let ph = match c.phase.as_str() {
                "Bound" => St::Ok,
                "Pending" => St::Warn,
                _ => St::Crit,
            };
            rows.push(vec![
                seg(format!("{:<12} ", trunc_cell(&c.namespace, 12)), St::Dim),
                seg(format!("{:<22} ", trunc_cell(&c.name, 22)), St::Normal),
                seg(format!("{:<9} ", trunc_cell(&c.phase, 9)), ph),
                seg(format!("{:>9} ", c.capacity_bytes.map(format::fmt_bytes).unwrap_or_else(|| "-".to_string())), St::Normal),
                seg(trunc_cell(&c.class, 14), St::Dim),
            ]);
        }
    }
    if st.kube_pvcs.is_empty() {
        if let Some(why) = split_note(st.kube_note.as_deref(), "pvcs") {
            rows.push(vec![seg(format!("       storage claims unavailable: {}", trunc_cell(&why, 76)), St::Dim)]);
        }
    }
    rows
}

/// Millicores as humans read them: 88m small, 1.5c cores.
fn cpu_label(milli: u64) -> String {
    if milli >= 1000 {
        format!("{:.1}c", milli as f64 / 1000.0)
    } else {
        format!("{milli}m")
    }
}

/// Pull one sub-section's reason out of the joined note string (join_notes uses "; ").
fn split_note(note: Option<&str>, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    note?.split("; ").find_map(|seg| seg.strip_prefix(prefix.as_str())).map(str::to_string)
}

fn proc_table(view: &View, max_rows: usize) -> Vec<Row> {
    // Owner bug report 2026-10-11: with DGX2 focused, TOP PROCS led with fedora's Unity
    // Editor — the fleet union had always been drawn on EVERY station tab. A focused
    // station shows ITS OWN table (single machine ⇒ drop the redundant HOST column);
    // tabs without host telemetry (overview / PVE / K8S) keep the cross-fleet union.
    let focus_label = view.stations.get(view.focus).map(|s| s.label.as_str()).unwrap_or("");
    let scope_to_focus = view.stations.get(view.focus).and_then(|s| s.host).is_some();
    let mut procs: Vec<(&str, &crate::snapshot::ProcRow)> = Vec::new();
    for s in &view.stations {
        if s.label == OVERVIEW_LABEL {
            continue;
        }
        if scope_to_focus && s.label != focus_label {
            continue;
        }
        if let Some(h) = s.host {
            for p in h.procs.iter() {
                procs.push((s.label.as_str(), p));
            }
        }
    }
    let multi_host = !scope_to_focus && view.stations.iter().filter(|s| s.host.is_some()).count() > 1;
    match view.sort_label {
        "MEM" => procs.sort_by(|a, b| b.1.mem_pct.total_cmp(&a.1.mem_pct)),
        "PID" => procs.sort_by_key(|(_, p)| p.pid),
        _ => procs.sort_by(|a, b| b.1.cpu_pct.total_cmp(&a.1.cpu_pct)),
    }

    let mut rows = vec![vec![
        seg(format!("TOP PROCS · sort {} · [s]ort [+/-]{}", view.sort_label, view.top), St::Head),
    ]];
    let head_cols = if multi_host {
        format!("{:<10} {:>7} {:<8} {:>5} {:>5} {:>8}  CMD", "HOST", "PID", "USER", "%CPU", "%MEM", "TIME")
    } else {
        format!("{:>7} {:<8} {:>5} {:>5} {:>8}  CMD", "PID", "USER", "%CPU", "%MEM", "TIME")
    };
    rows.push(vec![seg(head_cols, St::Head)]);

    let width = view.width as usize;
    for (host, p) in procs.into_iter().take(max_rows) {
        let cmd_cap = if multi_host { 30 } else { 40 };
        let base = if multi_host {
            format!("{:<10} {:>7} {:<8} {:>5.1} {:>5.1} {:>8}  ", trunc_cell(host, 10), p.pid, trunc_cell(&p.user, 8), p.cpu_pct, p.mem_pct, format::fmt_etime(p.elapsed_s))
        } else {
            format!("{:>7} {:<8} {:>5.1} {:>5.1} {:>8}  ", p.pid, trunc_cell(&p.user, 8), p.cpu_pct, p.mem_pct, format::fmt_etime(p.elapsed_s))
        };
        let cmd = trunc_cell(&p.cmd, cmd_cap);
        let st = if p.cpu_pct >= view.crit as f32 { St::Crit } else if p.cpu_pct >= view.warn as f32 { St::Warn } else { St::Normal };
        let _ = width;
        rows.push(vec![seg(base, St::Dim), seg(cmd, st)]);
    }
    rows
}

pub fn alert_lines(view: &View) -> Vec<Row> {
    let mut alerts: Vec<(St, String)> = Vec::new();
    for s in &view.stations {
        // Review fix #6: an errored sampler (ssh down, kubectl failing) is an ALERT even
        // when last-known telemetry still looks healthy — silence on dead links misled.
        if let Some(e) = &s.err {
            alerts.push((St::Crit, format!("{} LINK DOWN: {}", s.title(), trunc_cell(e, 52))));
        }
        if let Some(h) = s.host {
            for d in &h.disks {
                if d.pct() >= view.warn {
                    alerts.push((sev_of(d.pct(), view.warn, view.crit), {
                        let where_ = if d.mount == "/" { s.title() } else { format!("{}{}", s.title(), d.mount) };
                        format!("DISK {} {:.0}% USED ({})", where_, d.pct(), format::fmt_bytes(d.avail_bytes))
                    }));
                }
            }
            if h.mem_pct() >= view.warn {
                alerts.push((sev_of(h.mem_pct(), view.warn, view.crit), format!("{} MEM {:.0}% USED", s.title(), h.mem_pct())));
            }
            for g in &h.gpus {
                if g.vram_pct() >= view.warn {
                    alerts.push((sev_of(g.vram_pct(), view.warn, view.crit), format!("{} NVRAM {:.0}% USED", s.title(), g.vram_pct())));
                }
            }
            if h.cpu_avg_pct as f64 >= view.warn {
                alerts.push((sev_of(h.cpu_avg_pct as f64, view.warn, view.crit), format!("{} CPU {:.0}% UTIL", s.title(), h.cpu_avg_pct)));
            }
        }
        if let Some(nodes) = s.kube {
            for n in nodes.iter().filter(|n| !kube_ready(&n.status)) {
                alerts.push((St::Crit, format!("NODE {} NOT READY ({})", n.name, n.status)));
            }
        }
    }
    // Proxmox alarms share the crit-first sort below; one source list feeds lines AND count.
    alerts.extend(pve_alerts(view, view.warn));
    alerts.sort_by_key(|(s, _)| match s { St::Crit => 0, St::Warn => 1, _ => 2 });
    let mut rows = Vec::new();
    for (st, msg) in alerts.iter().take(2) {
        rows.push(vec![seg(format!(">>  ALERT  {msg}  <<"), *st)]);
    }
    if alerts.len() > 2 {
        rows.push(vec![seg(format!(">>  +{} more alert(s)", alerts.len() - 2), St::Warn)]);
    }
    rows
}

pub fn alert_count(view: &View) -> usize {
    let mut n = 0usize;
    for s in &view.stations {
        // An active station error counts as one alert (review fix #6).
        if s.err.is_some() {
            n += 1;
        }
        if let Some(h) = s.host {
            n += h.disks.iter().filter(|d| d.pct() >= view.warn).count();
            if h.mem_pct() >= view.warn {
                n += 1;
            }
            n += h.gpus.iter().filter(|g| g.vram_pct() >= view.warn).count();
            if h.cpu_avg_pct as f64 >= view.warn {
                n += 1;
            }
        }
        if let Some(nodes) = s.kube {
            n += nodes.iter().filter(|n| !kube_ready(&n.status)).count();
        }
    }
    n += pve_alerts(view, view.warn).len();
    n
}

fn footer_rows(view: &View, alerts: usize, worst: St) -> Vec<Row> {
    let w = view.width as usize;
    let status = if alerts == 0 {
        vec![seg("[o] ALL STATIONS NOMINAL", St::Ok)]
    } else {
        vec![seg(format!("!{alerts:02} ACTIVE ALERTS"), worst)]
    };
    let hints = if view.paused {
        "[q]uit  [SPACE] resume  — PAUSED"
    } else {
        "[q]uit  [TAB] tab  [+/-] rows  [s]ort  [v]erbose  [SPACE] pause"
    };
    vec![
        rule(w),
        // Status first, version second, key hints last — on a narrow terminal the HINTS
        // absorb truncation, never the alert count (the old order cut mid-status at 96 cols).
        status
            .into_iter()
            .chain([
                seg("  ".to_string(), St::Normal),
                seg(format!("ccc v{VERSION} {spin}", spin = view.spin), St::Dim),
                seg("  ·  ".to_string(), St::Rule),
                seg(hints, St::Dim),
            ])
            .collect(),
    ]
}

fn tab_bar(view: &View) -> Row {
    let mut r = Vec::new();
    for (i, s) in view.stations.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, trunc_cell(&s.title(), 16));
        r.push(seg(if i == view.focus { format!("[{label}]") } else { label }, if i == view.focus { St::Title } else { St::Dim }));
        if i + 1 < view.stations.len() {
            r.push(seg("│", St::Rule));
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Build the screen: banner, tab bar, focused-station panels, global tables, footer.
///
/// Review fix #17: the budget model is explicit. Every entry carries a drop priority and a
/// protected flag; the shrink pass removes the BOTTOM-MOST unprotected row of the highest
/// group first, so the banner, tabs, section/column headers, alert lines and footer survive
/// at small heights that previously ate the status line (`out.truncate()` cut the footer).
pub fn build_rows(view: &View) -> Vec<Row> {
    // Round-2 finding 19: the old floors (w.max(30)/h.max(12)) laid out a frame LARGER
    // than the terminal, so `--width 20 --height 5` printed 12 truncated rows and the
    // live renderer clipped the footer away. Budget against the REAL dimensions — every
    // cell factory (bar/core_strip/rate_cell) degrades gracefully, and finish_row clamps
    // any segment that exceeds width. This is the compact status-only layout for tiny frames.
    let w = view.width as usize;
    let h = view.height as usize;

    // Drop order under height pressure (highest number drops first):
    const P_PROC_ROW: u8 = 6;
    const P_DISK_ROW: u8 = 5;
    const P_KUBE_ROW: u8 = 4;
    const P_CHIP: u8 = 4;
    const P_ALERT: u8 = 3;
    const P_BODY: u8 = 2; // panel detail rows (cores/util/bars) — keep titles, drop bodies
    const P_STRUCT: u8 = 0; // banner/tabs/rules/headers/footer: never dropped

    // (priority, protected?, row)
    let mut out: Vec<(u8, bool, Row)> = Vec::new();
    for r in header_rows(view) {
        out.push((P_STRUCT, true, r));
    }
    out.push((P_STRUCT, true, tab_bar(view)));
    out.push((P_STRUCT, true, rule(w)));

    let station = view.stations.get(view.focus).and_then(|s| s.host);
    let focus = &view.stations[view.focus.min(view.stations.len().saturating_sub(1))];
    let hist_cpu: &[f64] = &focus.hist_cpu;
    let hist_mem: &[f64] = &focus.hist_mem;

    // Fleet summary placement (owner directive 2026-10-11): `overview` mode lives on its
    // own tab so every station page is pure detail; `all` keeps the legacy strip on every
    // multi-station tab. The overview tab carries the disk column (consolidated dashboard).
    let focusing_overview = view.stations.get(view.focus).is_some_and(|s| s.label == OVERVIEW_LABEL);
    let show_chips = match view.summary {
        crate::config::SummaryMode::Off => false,
        crate::config::SummaryMode::All => view.stations.len() > 1,
        crate::config::SummaryMode::Overview => view.stations.len() > 1 && focusing_overview,
    };
    if show_chips {
        for r in fleet_chips(view, focusing_overview) {
            out.push((P_CHIP, false, r));
        }
    }
    for (i, r) in kube_table(view).into_iter().enumerate() {
        out.push((if i == 0 { P_STRUCT } else { P_KUBE_ROW }, i == 0, r));
    }
    for (i, r) in pve_table(view).into_iter().enumerate() {
        out.push((if i == 0 { P_STRUCT } else { P_KUBE_ROW }, i == 0, r));
    }
    match (station, focus.err.as_deref()) {
        (Some(stn), err) => {
            // A focused station with BOTH telemetry and an active error shows the STALE
            // line above its last-known panels (review fix #6).
            if let Some(e) = err {
                out.push((P_ALERT, true, vec![seg(format!("!! {} : {e}", focus.label), St::Crit)]));
            }
            for (j, r) in cpu_panel(view, stn, hist_cpu).into_iter().enumerate() {
                out.push((if j == 0 { P_STRUCT } else { P_BODY }, j == 0, r));
            }
            for (j, r) in mem_panel(view, stn, hist_mem).into_iter().enumerate() {
                out.push((if j == 0 { P_STRUCT } else { P_BODY }, j == 0, r));
            }
            for (j, r) in nvram_panel(view, stn).into_iter().enumerate() {
                // NVRAM name+bar pairs: drop bars (odd rows) before name/title rows.
                out.push((if j % 2 == 0 { P_STRUCT } else { P_BODY }, j % 2 == 0, r));
            }
            // Round-2 finding 28: --verbose must actually raise the interface cap (it used
            // to receive usize::MAX unconditionally, making the `v` key a no-op here).
            let net_cap = if view.verbose { usize::MAX } else { 8 };
            let mut it = net_panel(view, stn, net_cap).into_iter();
            if let Some(title) = it.next() {
                out.push((P_STRUCT, true, title));
            }
            for (j, r) in it.enumerate() {
                // Round-2 finding 7 (the inversion): interface detail rows are UNPROTECTED.
                // The busiest interface (j==0) drops LAST (P_BODY), secondary ones earlier
                // (P_CHIP) — protecting them let idle IFACE rows push disks, alerts, and
                // process rows off screen while showing 0 B/s traffic.
                out.push((if j < 1 { P_BODY } else { P_CHIP }, false, r));
            }
        }
        (None, Some(e)) => out.push((P_STRUCT, true, vec![seg(format!("!! {e}"), St::Crit)])),
        (None, None) => {}
    }
    if let Some(stn) = station {
        // Same as NET: verbose raises the disk cap from 8 to all mounts (finding 28).
        let disk_cap = if view.verbose { usize::MAX } else { 8 };
        for r in disk_panel(view, stn, disk_cap) {
            out.push((P_DISK_ROW, false, r));
        }
    }
    {
        // TOP PROCS renders on every tab (owner bug report 2026-10-11): scoped to the
        // focused station when it has host telemetry, fleet union on overview/PVE/K8S tabs.
        // Finding 29: honor --top 1 (main.rs clamps the range to 1..=40; the renderer used
        // to silently floor at 2, contradicting both the CLI and the [s]ort [+/-] hints.
        let mut it = proc_table(view, view.top.max(1)).into_iter();
        if let Some(t) = it.next() {
            out.push((P_STRUCT, true, t));
        }
        if let Some(hd) = it.next() {
            out.push((P_STRUCT, true, hd));
        }
        for r in it {
            out.push((P_PROC_ROW, false, r));
        }
    }
    let alert_rows = alert_lines(view);
    // Rows are single-style; the alert list is crit-first sorted so [0][0] carries the worst.
    let worst = alert_rows.first().and_then(|r| r.first()).map(|s| s.s).unwrap_or(St::Ok);
    for r in alert_rows {
        out.push((P_ALERT, false, r));
    }
    for r in footer_rows(view, alert_count(view), worst) {
        out.push((P_STRUCT, true, r));
    }

    // Shrink: drop the bottom-most UNPROTECTED row of the highest non-empty priority
    // until everything fits. Protected rows (titles, headers, footer) always survive;
    // in the pathological many-GPU/tiny-terminal case we keep the tail so the footer,
    // which carries the ALERT count, is what stays on screen.
    while out.len() > h {
        let mut victim = None;
        'find: for prio in (1..=6).rev() {
            for (i, (p, prot, _)) in out.iter().enumerate().rev() {
                if *p == prio && !*prot {
                    victim = Some(i);
                    break 'find;
                }
            }
        }
        match victim {
            Some(i) => {
                out.remove(i);
            }
            None => break,
        }
    }
    if out.len() > h {
        // Last resort: keep the bottom slice so the footer survives.
        let start = out.len() - h;
        out.drain(0..start);
    }

    out.drain(..).map(|(_, _, row)| finish_row(row, w)).collect()
}

// ---------------------------------------------------------------------------
// Output conversion
// ---------------------------------------------------------------------------

pub fn to_plain(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|r| {
        let s: String = r.iter().map(|seg| seg.t.as_str()).collect();
        s.trim_end().to_string()
    }).collect()
}

pub fn to_ratatui(rows: &[Row], no_color: bool) -> Vec<Line<'static>> {
    rows.iter()
        .map(|r| {
            Line::from(
                r.iter()
                    .map(|sg| {
                        if no_color {
                            Span::raw(sg.t.clone())
                        } else {
                            let mut style = Style::default().fg(sg.s.color());
                            if sg.s == St::Title || sg.s == St::Head {
                                style = style.add_modifier(Modifier::BOLD);
                            }
                            Span::styled(sg.t.clone(), style)
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_smooth_width_and_fill_contract() {
        assert_eq!(bar(0.0, 9), " ".repeat(9));
        assert_eq!(bar(100.0, 9), "█".repeat(9));
        assert_eq!(bar(-5.0, 10), bar(0.0, 10));
        assert_eq!(bar(150.0, 10), bar(100.0, 10));
        assert_eq!(visible_width(&bar(f64::NAN, 10)), 10);
        // Exact cell width at every size/value, first char full for >=1/w of usage.
        for w in [8, 9, 10, 20, 25] {
            for p in [0.0, 12.0, 33.3, 63.4, 91.0, 100.0] {
                assert_eq!(visible_width(&bar(p, w)), w, "bar({p},{w})");
            }
        }
        assert!(bar(55.0, 10).starts_with("█████")); // >= half full
        assert!(!bar(55.0, 10).contains('░'));      // no blocky filler left
    }

    #[test]
    fn core_strip_maps_height_ramp() {
        let s = core_strip(&[0.0, 12.0, 63.0, 91.0], 32);
        // height = floor(util / 12.5%) on the ▁..█ ramp
        assert_eq!(s, "·▁▆█");
        let capped = core_strip(&[10.0; 40], 16);
        assert!(capped.ends_with("+24"));
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(sev_of(59.9, 60.0, 85.0), St::Ok);
        assert_eq!(sev_of(60.0, 60.0, 85.0), St::Warn);
        assert_eq!(sev_of(85.0, 60.0, 85.0), St::Crit);
        assert_eq!(lamp(St::Crit), "[X]");
        assert_eq!(lamp(St::Ok), "[o]");
    }

    #[test]
    fn row_width_is_exact() {
        let row = vec![seg("日本語テキスト", St::Normal), seg("abc", St::Dim)];
        let fitted = finish_row(row, 10);
        assert_eq!(visible_width(&fitted.iter().map(|s| s.t.clone()).collect::<String>()), 10);
        let wide = finish_row(vec![seg("x".repeat(50), St::Normal)], 12);
        assert_eq!(visible_width(&wide.iter().map(|s| s.t.clone()).collect::<String>()), 12);
    }

    #[test]
    fn spark_pads_short_history() {
        assert_eq!(spark(&[50.0, 100.0], 4), "  ▅█");
        assert_eq!(spark(&[], 3), "   ");
    }
}

#[cfg(test)]
mod fleet_render_tests {
    use super::*;
    use crate::snapshot::{KubeNode, KubePod, KubePvc};

    fn node(name: &str, status: &str, cpu: Option<f32>) -> KubeNode {
        KubeNode {
            name: name.into(),
            status: status.into(),
            roles: "worker".into(),
            age: "210d".into(),
            version: "v1.36.2+k3s1".into(),
            cpu_millis: cpu.map(|v| v as u64 * 100),
            cpu_pct: cpu,
            mem_bytes: Some(2 << 30),
            mem_pct: Some(30.0),
        }
    }

    fn view_with_fleet<'a>(width: u16, nodes: &'a [KubeNode], pods: &'a [KubePod], pvcs: &'a [KubePvc]) -> View<'a> {
        View {
            clock: "00:00:00Z".into(),
            epoch: 0,
            spin: '*',
            stations: vec![UiStation {
                label: "K8S:test".into(),
                host: None,
                err: None,
                pve: None,
                kube: Some(nodes),
                kube_pods: pods,
                kube_pvcs: pvcs,
                kube_note: None,
                hist_cpu: Vec::new(),
                hist_mem: Vec::new(),
                sample_ts: 0,
            }],
            focus: 0,
            summary: crate::config::SummaryMode::All,
            sort_label: "CPU",
            top: 10,
            verbose: false,
            paused: false,
            warn: 60.0,
            crit: 85.0,
            width,
            height: 44,
        }
    }

    fn fixtures() -> (Vec<KubeNode>, Vec<KubePod>, Vec<KubePvc>) {
        (
            vec![node("fed-node-1.lab.local", "Ready", Some(11.0)), node("fed-node-2", "NotReady", Some(77.0))],
            vec![KubePod { namespace: "default".into(), pod: "web-7f9c-abc".into(), container: "app".into(), cpu_milli: 1200, mem_bytes: 88 << 20 }],
            vec![KubePvc { namespace: "db".into(), name: "postgres-data-0".into(), phase: "Bound".into(), capacity_bytes: Some(10 << 30), class: "local-path".into(), volume: "pvc-abc".into() }],
        )
    }

    #[test]
    fn fleet_panel_renders_bars_top_pods_and_pvcs() {
        let (nodes, pods, pvcs) = fixtures();
        let text = to_plain(&build_rows(&view_with_fleet(120, &nodes, &pods, &pvcs))).join("\n");
        // Node rows carry inline bar+percent cells (owner report: fleet tab was all text).
        let n1 = text.lines().find(|l| l.contains("fed-node-1")).expect("node row");
        assert!(n1.contains("11%"), "cpu percent visible: {n1}");
        assert!(n1.chars().any(|c| c == '░' || c == '█'), "bar glyphs present: {n1}");
        // NotReady node keeps its red status column value verbatim.
        assert!(text.lines().any(|l| l.contains("fed-node-2") && l.contains("NotReady")));
        // Sections appear with headers and data.
        assert!(text.contains("TOP PODS"), "{text}");
        assert!(text.contains("web-7f9c-abc") && text.contains("1.2c"));
        assert!(text.contains("PVC · 1/1 bound · 10.0 GiB provisioned"), "{text}");
        assert!(text.contains("postgres-data-0") && text.contains("local-path"));
        // Right-rail gauges roll up the fleet average ((11+77)/2 = 44% CPU, 30% MEM).
        assert!(text.contains("44%"), "fleet average cpu in rail: {text}");
        assert!(text.contains("NODES 1/2"), "ready chip: {text}");
    }

    #[test]
    fn narrow_terminal_keeps_numeric_gauges_never_blanks() {
        // The exact regression owner hit: a terminal narrower than the bar layout must
        // degrade to numeric readouts, NOT drop the rails entirely.
        let (nodes, pods, pvcs) = fixtures();
        let text = to_plain(&build_rows(&view_with_fleet(30, &nodes, &pods, &pvcs))).join("\n");
        assert!(text.contains("CPU 44%"), "{text}");
        assert!(text.contains("MEM 30%"), "{text}");
    }

    /// host_station() with a custom display name (multi-station render tests).
    fn host_station_named(label: &str) -> Station {
        Station {
            name: label.into(),
            ..host_station()
        }
    }

    fn host_station() -> Station {
        Station {
            name: "fedora".into(),
            ts_epoch: 0,
            os_name: "Fedora Linux 43".into(),
            kernel: "7.2.6-100.fc43.x86_64".into(),
            uptime_s: 14340,
            load: [41.9, 39.6, 27.9],
            core_count: 32,
            cpu_avg_pct: 58.0,
            per_core_pct: vec![58.0; 32],
            mem_total_kb: 94_100_000,
            mem_avail_kb: 66_100_000,
            swap_total_kb: 0,
            swap_free_kb: 0,
            disks: Vec::new(),
            net: Vec::new(),
            gpus: Vec::new(),
            procs: Vec::new(),
        }
    }

    #[test]
    fn fleet_panels_stay_on_the_fleet_tab() {
        // Owner screenshot 2026-10-11: the `fedora` tab rendered FLEET NODE / TOP PODS / PVC
        // above the local gauges because kube_table looked up the first station WITH k8s
        // data instead of the focused one. The fleet panels must follow focus exactly.
        let (nodes, pods, pvcs) = fixtures();
        let host = host_station();
        let build = |focus: usize| View {
            clock: "00:00:00Z".into(),
            epoch: 0,
            spin: '*',
            stations: vec![
                UiStation {
                    label: "fedora".into(),
                    host: Some(&host),
                    err: None,
                    pve: None,
                    kube: None,
                    kube_pods: &[],
                    kube_pvcs: &[],
                    kube_note: None,
                    hist_cpu: Vec::new(),
                    hist_mem: Vec::new(),
                    sample_ts: 0,
                },
                UiStation {
                    label: "K8S:homelab".into(),
                    host: None,
                    err: None,
                    pve: None,
                    kube: Some(&nodes),
                    kube_pods: &pods,
                    kube_pvcs: &pvcs,
                    kube_note: None,
                    hist_cpu: Vec::new(),
                    hist_mem: Vec::new(),
                    sample_ts: 0,
                },
            ],
            focus,
            summary: crate::config::SummaryMode::All,
            sort_label: "CPU",
            top: 10,
            verbose: false,
            paused: false,
            warn: 60.0,
            crit: 85.0,
            width: 160,
            height: 50,
        };
        let on_host = to_plain(&build_rows(&build(0))).join("\n");
        assert!(on_host.contains("fedora"), "host panels render: {on_host}");
        assert!(!on_host.contains("FLEET "), "no fleet table on the host tab: {on_host}");
        assert!(!on_host.contains("TOP PODS"), "no pod table on the host tab: {on_host}");
        assert!(!on_host.contains("PVC ·"), "no PVC section on the host tab: {on_host}");
        assert!(!on_host.contains("k3s-agent-00"), "no node rows leak: {on_host}");
        let on_fleet = to_plain(&build_rows(&build(1))).join("\n");
        assert!(on_fleet.contains("FLEET ") && on_fleet.contains("TOP PODS") && on_fleet.contains("PVC ·"), "fleet tab keeps its panels: {on_fleet}");
    }

    #[test]
    fn proc_table_scopes_to_the_focused_station() {
        // Owner bug report 2026-10-11 (screenshot): the DGX2 tab led TOP PROCS with fedora's
        // Unity Editor — the fleet union was drawn on every station tab. A focused station
        // shows ONLY its own processes; the Overview tab keeps the cross-fleet union.
        use crate::snapshot::ProcRow;
        let mk = |name: &str, pid: u32, cpu: f32, cmd: &str| Station {
            procs: vec![ProcRow { pid, user: name.into(), cpu_pct: cpu, mem_pct: 1.0, rss_kb: 1 << 20, elapsed_s: 60, cmd: cmd.into() }],
            ..host_station_named(name)
        };
        let alpha = mk("alpha", 10, 90.0, "alpha-trainer");
        let beta = mk("beta", 20, 40.0, "beta-inference");
        let view = |focus: usize, summary: crate::config::SummaryMode| View {
            clock: "00:00:00Z".into(),
            epoch: 0,
            spin: '*',
            stations: vec![
                UiStation { label: "overview".into(), host: None, err: None, pve: None, kube: None, kube_pods: &[], kube_pvcs: &[], kube_note: None, hist_cpu: Vec::new(), hist_mem: Vec::new(), sample_ts: 0 },
                UiStation { label: "alpha".into(), host: Some(&alpha), err: None, pve: None, kube: None, kube_pods: &[], kube_pvcs: &[], kube_note: None, hist_cpu: Vec::new(), hist_mem: Vec::new(), sample_ts: 0 },
                UiStation { label: "beta".into(), host: Some(&beta), err: None, pve: None, kube: None, kube_pods: &[], kube_pvcs: &[], kube_note: None, hist_cpu: Vec::new(), hist_mem: Vec::new(), sample_ts: 0 },
            ],
            focus,
            summary,
            sort_label: "CPU",
            top: 10,
            verbose: false,
            paused: false,
            warn: 60.0,
            crit: 85.0,
            width: 140,
            height: 50,
        };
        let on_alpha = to_plain(&build_rows(&view(1, crate::config::SummaryMode::Overview))).join("\n");
        assert!(on_alpha.contains("alpha-trainer"), "focused station lists its own process: {on_alpha}");
        assert!(!on_alpha.contains("beta-inference"), "other stations' processes must NOT leak into a station tab: {on_alpha}");
        // Focusing the busiest row alone also proves the table didn't just render unsorted.
        assert!(on_alpha.find("alpha-trainer").unwrap() < on_alpha.len());
        let on_beta = to_plain(&build_rows(&view(2, crate::config::SummaryMode::Overview))).join("\n");
        assert!(on_beta.contains("beta-inference") && !on_beta.contains("alpha-trainer"), "beta tab shows only beta: {on_beta}");
        let on_overview = to_plain(&build_rows(&view(0, crate::config::SummaryMode::Overview))).join("\n");
        assert!(on_overview.contains("alpha-trainer") && on_overview.contains("beta-inference"), "overview aggregates both: {on_overview}");
    }

    fn pve_fixture() -> crate::snapshot::PveCluster {
        use crate::snapshot::{PveGuest, PveNode, PveStorage};
        crate::snapshot::PveCluster {
            nodes: vec![
                PveNode { name: "node1".into(), ip: "192.0.2.241".into(), online: true, cpu_pct: Some(23.06), max_cpu: 12, mem_bytes: (39.34 * 1024f64.powi(3)) as u64, max_mem_bytes: (62.48 * 1024f64.powi(3)) as u64, uptime_s: 370_000 },
                PveNode { name: "node5".into(), ip: "192.0.2.245".into(), online: false, cpu_pct: None, max_cpu: 16, mem_bytes: 0, max_mem_bytes: 0, uptime_s: 0 },
            ],
            guests: vec![
                PveGuest { vmid: 101, kind: "qemu".into(), name: "k3s-agent-02".into(), node: "node3".into(), status: "running".into(), template: false, cpu_pct: Some(29.3), max_cpu: 8, mem_bytes: 32 << 30, max_mem_bytes: 32 << 30 },
                PveGuest { vmid: 100, kind: "qemu".into(), name: "template-vm-rhel10".into(), node: "node1".into(), status: "stopped".into(), template: true, cpu_pct: Some(0.0), max_cpu: 4, mem_bytes: 0, max_mem_bytes: 8 << 30 },
            ],
            storage: vec![
                PveStorage { node: "node1".into(), name: "nfs-backup".into(), kind: "nfs".into(), content: "backup,vztmpl,iso".into(), total_bytes: 67 << 30, avail_bytes: 60 << 30, used_bytes: 4 << 30, used_pct: Some(6.28) },
                PveStorage { node: "node1".into(), name: "ceph_pool01".into(), kind: "rbd".into(), content: "rootdir,images".into(), total_bytes: 2 << 40, avail_bytes: 933 << 20, used_bytes: (1.02 * 1024f64.powi(4)) as u64, used_pct: Some(52.82) },
            ],
            ceph_health: Some("HEALTH_WARN".into()),
            notes: vec!["quorum OK".into()],
        }
    }

    #[test]
    fn pve_panel_renders_nodes_guests_storage_ceph() {
        let c = pve_fixture();
        let view = View {
            clock: "00:00:00Z".into(),
            epoch: 0,
            spin: '*',
            stations: vec![UiStation {
                label: "PVE:lab".into(),
                host: None,
                err: None,
                pve: Some(&c),
                kube: None,
                kube_pods: &[],
                kube_pvcs: &[],
                kube_note: None,
                hist_cpu: Vec::new(),
                hist_mem: Vec::new(),
                sample_ts: 0,
            }],
            focus: 0,
            summary: crate::config::SummaryMode::All,
            sort_label: "CPU",
            top: 10,
            verbose: false,
            paused: false,
            warn: 60.0,
            crit: 85.0,
            width: 150,
            height: 50,
        };
        let text = to_plain(&build_rows(&view)).join("\n");
        // Header meta + chip come from cluster rollup.
        assert!(text.contains("PVE CLUSTER · 1/2 NODES ONLINE · 1/2 GUESTS RUNNING"), "{text}");
        assert!(text.contains("PVE 1/2"), "top-right chip: {text}");
        // Node rows with bars; offline node flagged in caps.
        assert!(text.contains("node1") && text.contains("192.0.2.241"), "node row: {text}");
        assert!(text.contains("OFFLINE"), "offline node shouts: {text}");
        // Guests ranked; the hot VM before the stopped template.
        assert!(text.contains("GUESTS · 1 of 2 running"), "{text}");
        let i_agent = text.find("k3s-agent-02").expect("guest row");
        let i_tpl = text.find("template-vm-rhel10").expect("template row");
        assert!(i_agent < i_tpl, "running-first ordering: {text}");
        assert!(text.contains(" T"), "template marker: {text}");
        // Storage sorted by fill; the 52.8% pool above the 6.3% NFS mount.
        assert!(text.contains("STORAGE · 2 volumes"), "{text}");
        assert!(text.contains("ceph_pool01") && text.contains("53%"), "{text}");
        assert!(text.contains("nfs-backup") && text.contains("6%"), "{text}");
        let i_pool = text.find("ceph_pool01").unwrap();
        let i_nfs = text.find("nfs-backup").unwrap();
        assert!(i_pool < i_nfs, "hottest volume first");
        // Ceph health shown verbatim and red for non-OK.
        assert!(text.contains("health HEALTH_WARN"), "{text}");
        assert!(text.contains("CEPH HEALTH HEALTH_WARN"), "alert raised: {text}");
        // Collection note rendered.
        assert!(text.contains("quorum OK"), "{text}");
    }

    #[test]
    fn pve_panels_stay_on_the_pve_tab() {
        // Companion of fleet_panels_stay_on_the_fleet_tab: with three tabs open, the
        // hypervisor tables must render ONLY when the PVE slot is focused.
        let c = pve_fixture();
        let host = host_station();
        let build = |focus: usize| View {
            clock: "00:00:00Z".into(),
            epoch: 0,
            spin: '*',
            stations: vec![
                UiStation { label: "fedora".into(), host: Some(&host), err: None, pve: None, kube: None, kube_pods: &[], kube_pvcs: &[], kube_note: None, hist_cpu: Vec::new(), hist_mem: Vec::new(), sample_ts: 0 },
                UiStation { label: "PVE:lab".into(), host: None, err: None, pve: Some(&c), kube: None, kube_pods: &[], kube_pvcs: &[], kube_note: None, hist_cpu: Vec::new(), hist_mem: Vec::new(), sample_ts: 0 },
            ],
            focus,
            summary: crate::config::SummaryMode::All,
            sort_label: "CPU",
            top: 10,
            verbose: false,
            paused: false,
            warn: 60.0,
            crit: 85.0,
            width: 150,
            height: 50,
        };
        let on_host = to_plain(&build_rows(&build(0))).join("\n");
        // The GLOBAL alert banner legitimately names PVE nodes on any tab ("ALERT PVE NODE
        // node5 OFFLINE") — that's cross-fleet awareness, not the fleet panel. Assert on the
        // panel's own column header, which only exists in pve_table's output.
        assert!(!on_host.contains("PVE NODE    STATUS"), "no hypervisor TABLE on host tab: {on_host}");
        assert!(!on_host.contains("GUESTS ·"), "no guest table on host tab");
        assert!(!on_host.contains("STORAGE ·"), "no storage table on host tab");
        // (No ceph-panel assertion here: the GLOBAL alert banner prints "CEPH HEALTH …"
        // on every tab by design; pve_panel_renders… covers the panel's own presence.)
        let on_pve = to_plain(&build_rows(&build(1))).join("\n");
        assert!(on_pve.contains("PVE NODE") && on_pve.contains("GUESTS ·"), "pve tab keeps panels");
    }

    #[test]
    fn ceph_status_colors() {
        assert_eq!(crate::render::ceph_st("OK"), St::Ok);
        assert_eq!(crate::render::ceph_st("HEALTH_OK"), St::Ok);
        assert_eq!(crate::render::ceph_st("HEALTH_WARN"), St::Warn);
        assert_eq!(crate::render::ceph_st("HEALTH_ERR"), St::Crit);
    }

    #[test]
    fn cpu_label_cores_and_millis() {
        assert_eq!(cpu_label(88), "88m");
        assert_eq!(cpu_label(999), "999m");
        assert_eq!(cpu_label(1200), "1.2c");
        assert_eq!(split_note(Some("top failed; pods: kubectl exited 1: denied"), "pods").as_deref(), Some("kubectl exited 1: denied"));
        assert_eq!(split_note(None, "pods"), None);
        assert_eq!(split_note(Some("unrelated note"), "pods"), None);
    }
}
