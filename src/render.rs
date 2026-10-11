//! Presentation layer: turns UI state into styled `ratatui` lines (live mode) or plain
//! text (`--once`, README samples, tests). One line pipeline serves both paths, so what
//! you unit-test is what ships.
//!
//! Visual language follows the approved concept sheet (Codex Banner B): heavy C2 stencil
//! wordmark, segmented gauges with quarter ticks, per-core vertical strips, `[o]/[!]/[X]`
//! status lamps, `>>  WARN ... <<` chevrons, footer with station/uptime/load/clock/alerts.

use crate::format;
use crate::snapshot::{KubeNode, Station};
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

// Codex concept sheet fill ramp; index = round(fraction * 4) on the boundary cell.
const RAMP: [char; 5] = ['░', '▎', '▌', '▊', '█'];
const CORE_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Segmented horizontal gauge, exactly `width` cells, quarter-capacity tick marks.
/// Cell fill uses the quarter-ramp (`▏▎▌█`) so partial values stay honest.
pub fn bar(pct: f64, width: usize) -> String {
    let w = width.max(1);
    let p = if pct.is_finite() { pct.clamp(0.0, 100.0) } else { 0.0 };
    let exact = p / 100.0 * w as f64;
    let full = (exact.floor() as usize).min(w);
    let rem = exact - exact.floor();
    let mut chars: Vec<char> = (0..w)
        .map(|i| {
            if i < full {
                '█'
            } else if i == full {
                RAMP[(rem * 4.0).round().clamp(0.0, 3.0) as usize]
            } else {
                '░'
            }
        })
        .collect();
    // Quarter ticks overwrite the cell completing each quarter: 25% 50% 75%.
    for q in [w / 4, w / 2, (3 * w) / 4] {
        if q > 0 && q < w {
            chars[q - 1] = '|';
        }
    }
    chars.into_iter().collect()
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

/// Everything the renderer needs for one frame.
pub struct View<'a> {
    pub clock: String,
    /// Wall-clock Unix seconds at draw time — enables sample-age display (review fix #6).
    pub epoch: i64,
    pub spin: char,
    pub stations: Vec<UiStation<'a>>,
    pub focus: usize,
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
    pub kube: Option<&'a [KubeNode]>,
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

    let right_gauge = |label: &str, pct: f64| -> Row {
        let mut r = mini_gauge(label, pct, view, 8);
        // Right rail is fixed width; drop the percent for narrow widths handled by pad.
        r.extend(vec![seg(" ", St::Normal)]);
        r
    };

    let station_label = cur.map(|s| s.title()).unwrap_or_default();
    // Fleet slots have no Station/HostInfo, so they previously rendered the same
    // "NO TELEMETRY / station offline" pair as a dead link — while the panel below was
    // demonstrably full of live node rows. Give them their own meta line.
    let meta = cur
        .and_then(|s| s.host)
        .map(|h| format!("UP {} · LOAD {:.1} {:.1} {:.1}", format::fmt_uptime(h.uptime_s), h.load[0], h.load[1], h.load[2]))
        .or_else(|| {
            cur.and_then(|s| s.kube).map(|k| {
                let ready = k.iter().filter(|n| kube_ready(&n.status)).count();
                format!("FLEET ONLINE · {ready}/{} NODES READY", k.len())
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
                .unwrap_or_else(|| match cur.and_then(|s| s.kube) {
                    Some(k) => vec![
                        seg("kubectl fleet", St::Dim),
                        seg(" · ", St::Rule),
                        seg(format!("{} node{} via kubeconfig", k.len(), if k.len() == 1 { "" } else { "s" }), St::Dim),
                    ],
                    None => vec![seg("station offline".to_string(), St::Warn)],
                }),
            4 => vec![seg(view.clock.clone(), St::Dim), seg("  ", St::Normal), seg(format!("[{}] SAMPLE", view.spin), St::Ident)],
            _ => vec![],
        };
        r.extend(mid);
        rows.push(r);
    }
    // Right rail gauges for the focused station (rows 1..5).
    if w >= 58 {
        if let Some(s) = cur {
            let rails: Vec<(&str, f64)> = match s.host {
                Some(h) => {
                    let nvram = gpu_avg_pct(h);
                    vec![("CPU", h.cpu_avg_pct as f64), ("MEM", h.mem_pct()), ("NVRAM", nvram), ("DISK", disk_max_pct(h))]
                }
                // Fleet slots roll up `kubectl top` usage-vs-allocatable into the same bars;
                // nodes without a metric are skipped so metrics-server gaps can't read as 0%.
                None => match s.kube {
                    Some(k) => {
                        let avg = |get: fn(&KubeNode) -> Option<f32>| -> f64 {
                            let v: Vec<f64> = k.iter().filter_map(get).map(f64::from).collect();
                            if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
                        };
                        vec![
                            ("CPU", avg(|n| n.cpu_pct)),
                            ("MEM", avg(|n| n.mem_pct)),
                            ("NVRAM", 0.0),
                            ("DISK", 0.0),
                        ]
                    }
                    None => vec![("CPU", 0.0), ("MEM", 0.0), ("NVRAM", 0.0), ("DISK", 0.0)],
                },
            };
            for (i, (label, pct)) in rails.iter().enumerate() {
                let row = &mut rows[1 + i];
                append_right_aligned(row, &right_gauge(label, *pct), w);
            }
            if let Some(k) = s.kube {
                let ready = k.iter().filter(|n| kube_ready(&n.status)).count();
                append_right_aligned(
                    &mut rows[0],
                    &[seg(format!("NODES {ready}/{}", k.len()), St::Ident)],
                    w,
                );
            }
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
        rows.push(vec![
            seg("VRAM ", St::Dim),
            seg("[", St::Rule),
            seg(bar(g.vram_pct(), (w / 3).max(10)), st),
            seg("]", St::Rule),
            seg(
                format!(
                    " {:.1}/{:.1} GiB ({:.0}%) ",
                    g.vram_used_mib / 1024.0,
                    g.vram_total_mib / 1024.0,
                    g.vram_pct()
                ),
                st,
            ),
            seg(lamp(st), st),
        ]);
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

fn fleet_chips(view: &View) -> Vec<Row> {
    let mut rows = Vec::new();
    for (i, s) in view.stations.iter().enumerate() {
        if s.kube.is_some() {
            continue; // rendered by kube_table
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
                (
                    l,
                    format!(
                        "cpu {:>4.0}%  mem {:>4.0}%  nvram {:>4.0}%  load {:.1}",
                        h.cpu_avg_pct, h.mem_pct(), nv, h.load[0]
                    ),
                )
            }
            (None, Some(e)) => (St::Crit, trunc_cell(e, 40)),
            (None, None) => (St::Dim, "awaiting first sample…".to_string()),
        };
        rows.push(vec![
            seg(format!("[{}] ", i + 1), St::Dim),
            seg(lamp(lamp_st), lamp_st),
            seg(format!("{:<14}", trunc_cell(&s.title(), 14)), if i == view.focus { St::Ident } else { St::Normal }),
            seg(tail, lamp_st),
        ]);
    }
    rows
}

fn kube_table(view: &View) -> Vec<Row> {
    let mut rows = Vec::new();
    let Some(st) = view.stations.iter().find(|s| s.kube.is_some()) else {
        return rows;
    };
    let Some(nodes) = st.kube else { return rows };
    rows.push(vec![
        seg("FLEET ", St::Head),
        seg(format!("{:<22} {:<12} {:<14} {:>6} {:>6}", "NODE", "STATUS", "ROLES", "CPU%", "MEM%"), St::Head),
    ]);
    for n in nodes.iter().take(if view.verbose { 32 } else { 8 }) {
        // Review fix #23: readiness is one of the comma-joined conditions — a cordoned
        // "Ready,SchedulingDisabled" node is healthy, not an alarm.
        let st = if kube_ready(&n.status) { St::Ok } else { St::Crit };
        // Field separators must mirror the header format EXACTLY (round-2 era layout bug
        // caught in live k8s capture: header has spaces between columns, rows had none, so
        // a 22-char hostname rendered flush against "Ready").
        rows.push(vec![
            seg(format!("{:<22} ", trunc_cell(&n.name, 22)), St::Normal),
            // Round-2 finding 32: "Ready,SchedulingDisabled" is 22 cells — the header says
            // STATUS at 12; truncate to keep roles/metrics columns aligned.
            seg(format!("{:<12} ", trunc_cell(&n.status, 12)), st),
            seg(format!("{:<14} ", trunc_cell(&n.roles, 14)), St::Dim),
            seg(format!("{:>6} ", n.cpu_pct.map(|v| format!("{v:.0}%")).unwrap_or("-".into())), St::Normal),
            seg(format!("{:>6}", n.mem_pct.map(|v| format!("{v:.0}%")).unwrap_or("-".into())), St::Normal),
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
    rows
}

fn proc_table(view: &View, multi_host: bool, max_rows: usize) -> Vec<Row> {
    let mut procs: Vec<(&str, &crate::snapshot::ProcRow)> = Vec::new();
    for s in &view.stations {
        if let Some(h) = s.host {
            for p in h.procs.iter() {
                procs.push((s.label.as_str(), p));
            }
        }
    }
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
    let multi = view.stations.iter().filter(|s| s.host.is_some()).count() > 1;

    if view.stations.len() > 1 {
        for r in fleet_chips(view) {
            out.push((P_CHIP, false, r));
        }
    }
    for (i, r) in kube_table(view).into_iter().enumerate() {
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
        // Finding 29: honor --top 1 (main.rs clamps the range to 1..=40; the renderer used
        // to silently floor at 2, contradicting both the CLI and the [s]ort [+/-] hints.
        let mut it = proc_table(view, multi, view.top.max(1)).into_iter();
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
    fn bar_extremes_and_quarter_ticks() {
        let empty = bar(0.0, 20);
        assert_eq!(empty.matches('|').count(), 3); // quarter ticks 25/50/75%
        assert_eq!(empty.matches('░').count(), 17);
        assert_eq!(empty.chars().count(), 20);
        assert!(bar(100.0, 20).chars().all(|c| c == '█' || c == '|'));
        assert_eq!(bar(50.0, 20).chars().count(), 20);
        // Width must be exactly preserved for table alignment.
        for w in [8, 10, 12, 20, 25] {
            for p in [0.0, 12.0, 33.3, 63.4, 91.0, 100.0] {
                assert_eq!(visible_width(&bar(p, w)), w, "bar({p},{w})");
            }
        }
        // NaN/negative/incoming-garbage clamp safely.
        assert_eq!(visible_width(&bar(f64::NAN, 10)), 10);
        assert_eq!(visible_width(&bar(-5.0, 10)), 10);
        assert_eq!(visible_width(&bar(150.0, 10)), 10);
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
