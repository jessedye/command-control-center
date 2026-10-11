//! ccc — COMMAND CONTROL: a command-and-control terminal dashboard for local and remote machines.

mod app;
mod collect;
mod config;
mod format;
mod gpu;
mod kube;
mod proto;
mod render;
mod snapshot;

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(ValueEnum, Clone, Copy)]
enum SortArg {
    Cpu,
    Mem,
    Pid,
}

#[derive(Parser)]
#[command(
    name = "ccc",
    version,
    about = "COMMAND CONTROL — live ASCII command deck for your machines: CPU, memory, NVRAM, disks, network, top processes.",
    long_about = "A low-overhead TUI dashboard in the style of an operations command center.\n\
        Watch the local machine and any number of remote hosts (over your own ssh config)\n\
        plus Kubernetes node fleets, all as live ASCII gauges with tabs.\n\n\
        With no flags ccc reads ~/.config/ccc/ccc.conf (see docs/SETUP.md) so plain\n\
        'ccc' opens your named fleet: fedora, K8S:homelab, rack4 ... as tabs."
)]
struct Opts {
    /// Redraw + sample interval in seconds (0.25..). Overrides the config file's setting.
    #[arg(short = 'i', long)]
    interval: Option<f64>,

    /// Initial rows shown in the process table (+/- adjust live).
    #[arg(short = 't', long)]
    top: Option<usize>,

    /// Process table sort column at start (s cycles it live).
    #[arg(long, value_enum)]
    sort: Option<SortArg>,

    /// Label for the local station tab (default: short hostname).
    #[arg(long)]
    station: Option<String>,

    /// Add a remote station; repeatable. VALUE is ssh destination tokens, optionally
    /// labeled: "NAME=-p 2222 user@host". Given any -H/--k8s, the config file's station
    /// lines are ignored (CLI wins wholesale — no half-merged fleet surprises).
    #[arg(short = 'H', long = "host", value_name = "SPEC")]
    hosts: Vec<String>,

    /// Watch Kubernetes nodes via kubectl (`--k8s[=CONTEXT]`; bare flag = current context).
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "CONTEXT")]
    k8s: Option<String>,

    /// Render one frame to stdout (plain text, no color) and exit — for scripts, README samples, CI.
    #[arg(long)]
    once: bool,

    /// Frame width for --once (default: current terminal, else 92).
    #[arg(long, default_value_t = 0)]
    width: u16,

    /// Frame height for --once (default: current terminal, else 36).
    #[arg(long, default_value_t = 0)]
    height: u16,

    /// Disable ANSI color (also honors the NO_COLOR environment variable).
    #[arg(long)]
    no_color: bool,

    /// Percent utilization that flips gauges to WARN.
    #[arg(long)]
    warn: Option<f64>,

    /// Percent utilization that flips gauges to CRIT.
    #[arg(long)]
    crit: Option<f64>,

    /// Start expanded: more disk/interface rows.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Read stations and settings from FILE instead of the default config
    /// ($XDG_CONFIG_HOME/ccc/ccc.conf). Keeps several profiles — lab.conf, prod.conf —
    /// launchable as `ccc --config ~/ccc/lab.conf`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Ignore the default config file (stations from flags only).
    #[arg(long)]
    no_config: bool,

    /// Tab to open focused: label ("fedora"), label prefix ("K8S:"), or 1-based tab number.
    /// Opening with --k8s defaults to the fleet tab; a config 'focus' line sets the default
    /// for config-launched fleets.
    #[arg(long, value_name = "TARGET")]
    focus: Option<String>,
}

fn main() -> ExitCode {
    let opts = Opts::parse();

    // ---- config file (stations + fallback settings; CLI always wins) ----
    if opts.no_config && opts.config.is_some() {
        eprintln!("ccc: --config and --no-config are mutually exclusive");
        return ExitCode::from(2);
    }
    let file_cfg = match load_file_config(&opts) {
        Ok(cfg) => cfg,
        Err(msg) => {
            for line in msg {
                eprintln!("ccc: {line}");
            }
            return ExitCode::from(2);
        }
    };

    // ---- numeric settings: CLI > config file > built-in defaults ----
    let interval = opts.interval.unwrap_or(file_cfg.interval.unwrap_or(1.0));
    let top = opts.top.unwrap_or(file_cfg.top.unwrap_or(10));
    let mut warn = opts.warn.unwrap_or(file_cfg.warn.unwrap_or(60.0));
    let mut crit = opts.crit.unwrap_or(file_cfg.crit.unwrap_or(85.0));
    let verbose = opts.verbose || file_cfg.verbose;
    let no_color = opts.no_color || file_cfg.no_color;
    let sort_word = opts.sort.map(sort_name).map(str::to_string).or(file_cfg.sort.clone());

    // Central numeric validation (review fix #14): NaN slips past plain range comparisons
    // (all NaN ordering is false) and panicked later inside Duration::from_secs_f64.
    if !interval.is_finite() || !(0.25..=3600.0).contains(&interval) {
        eprintln!("ccc: interval must be a finite number in 0.25..=3600 seconds, got '{interval}'");
        return ExitCode::from(2);
    }
    for (name, v) in [("warn", &mut warn), ("crit", &mut crit)] {
        if !v.is_finite() || !(0.0..=100.0).contains(v) {
            eprintln!("ccc: {name} must be a finite percentage 0..=100, got '{v}'");
            return ExitCode::from(2);
        }
    }
    // Round-2 finding (last item): an explicit crit below warn used to be SILENTLY raised
    // by `crit.max(warn)`, so the operator's number had no effect. Fail loudly.
    if crit < warn {
        eprintln!("ccc: crit ({crit}) must be >= warn ({warn})");
        return ExitCode::from(2);
    }
    let sort = match sort_word.as_deref() {
        None | Some("cpu") => app::SortBy::Cpu,
        Some("mem") => app::SortBy::Mem,
        Some("pid") => app::SortBy::Pid,
        Some(other) => {
            eprintln!("ccc: sort must be cpu, mem, or pid, got '{other}'");
            return ExitCode::from(2);
        }
    };

    // ---- stations: any CLI station flag replaces the config's station lines entirely ----
    let cli_stations = !opts.hosts.is_empty() || opts.k8s.is_some();
    let mut remotes: Vec<app::RemoteSpec> = Vec::new();
    if cli_stations {
        for spec in &opts.hosts {
            // Quote-aware split (review fix #19) so `-i "my key"` survives the string form of -H.
            let tokens = match collect::shell_split(spec) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("ccc: invalid --host '{spec}': {e}");
                    return ExitCode::from(2);
                }
            };
            let (label, tokens) = split_label(&tokens);
            if let Some(l) = &label {
                if l.is_empty() {
                    eprintln!("ccc: invalid --host '{spec}': empty label before '='");
                    return ExitCode::from(2);
                }
            }
            if tokens.is_empty() {
                eprintln!("ccc: invalid --host '{spec}': no destination given");
                return ExitCode::from(2);
            }
            match collect::split_ssh_tokens(&tokens) {
                Ok((flags, host)) => {
                    let label = label.unwrap_or_else(|| collect::auto_label(&tokens));
                    remotes.push(app::RemoteSpec { label, host, ssh_args: flags });
                }
                Err(e) => {
                    eprintln!("ccc: invalid --host '{spec}': {e}");
                    return ExitCode::from(2);
                }
            }
        }
    } else {
        // Config-file stations were validated at parse time; rebuild the ssh argument
        // vector minus its label token.
        for (label, tokens, host) in &file_cfg.remotes {
            let args: Vec<String> = tokens.iter().skip(1).cloned().collect();
            remotes.push(app::RemoteSpec { label: label.clone(), host: host.clone(), ssh_args: args });
        }
    }

    let (kube, kube_label) = if opts.k8s.is_some() {
        (opts.k8s.clone(), None)
    } else if !cli_stations {
        (file_cfg.kube_ctx.clone(), file_cfg.kube_label.clone())
    } else {
        (None, None)
    };

    // ---- focus: explicit target > "opened with --k8s means open on the fleet" > first tab ----
    let mut tab_names = vec![opts.station.clone().or(file_cfg.local_label.clone()).unwrap_or_else(default_station)];
    tab_names.extend(remotes.iter().map(|r| r.label.clone()));
    if kube.is_some() {
        tab_names.push(app::fleet_tab_name(&kube, &kube_label));
    }
    let focus_target = opts.focus.clone().or(if opts.k8s.is_some() {
        // Launching with --k8s says "I want the fleet": drop straight onto its tab instead
        // of making the operator press TAB every startup. A --focus flag overrides this.
        Some("__FLEET__".to_string())
    } else {
        file_cfg.focus.clone()
    });
    let focus = resolve_focus(focus_target.as_deref(), &tab_names);

    let cfg = app::Config {
        interval,
        top: top.clamp(1, 40),
        sort,
        station_label: tab_names[0].clone(),
        no_color: no_color || std::env::var_os("NO_COLOR").is_some(),
        verbose,
        warn,
        crit,
        remotes,
        kube,
        kube_label,
        focus,
    };

    if opts.once {
        let (dw, dh) = shrink_size();
        let w = if opts.width > 0 { opts.width } else { dw };
        let h = if opts.height > 0 { opts.height } else { dh };
        print!("{}", app::snapshot_text(&cfg, w, h));
        println!();
        return ExitCode::SUCCESS;
    }

    match app::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccc: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Locate the config file: --config PATH (must exist), else the XDG default when present.
/// A missing DEFAULT file is silent (first run before any config exists is normal);
/// a missing EXPLICIT file is an operator typo worth shouting about.
fn load_file_config(opts: &Opts) -> Result<config::FileConfig, Vec<String>> {
    use std::io::Read as _;
    let path = match &opts.config {
        Some(p) => p.clone(),
        None => match config::default_path() {
            Some(p) if p.exists() => p,
            _ => return Ok(config::FileConfig::default()),
        },
    };
    let mut text = String::new();
    match std::fs::File::open(&path).and_then(|mut f| f.read_to_string(&mut text)) {
        Ok(_) => {}
        Err(e) => return Err(vec![format!("{}: {e}", path.display())]),
    }
    // UTF-8 BOM tolerance: a Windows-edited config must not fail on \u{feff}local.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
    let base = path.parent().unwrap_or(std::path::Path::new(""));
    config::parse(&text).map_err(|errs| {
        errs.into_iter()
            .map(|e| format!("{}: {e}", base.join(path.file_name().unwrap_or_default()).display()))
            .collect()
    })
}

fn sort_name(s: SortArg) -> &'static str {
    match s {
        SortArg::Cpu => "cpu",
        SortArg::Mem => "mem",
        SortArg::Pid => "pid",
    }
}

/// Resolve a focus target to a 0-based tab index. Accepts the internal "__FLEET__" marker
/// (set when launched with --k8s), exact label, case-insensitive prefix, substring, then a
/// 1-BASED tab number as shown on the tab bar. Unmatched targets fall back to tab 0 with a
/// warning printed BEFORE the terminal switches to the alt screen, so it stays readable.
fn resolve_focus(target: Option<&str>, tabs: &[String]) -> usize {
    let Some(t) = target else { return 0 };
    if t == "__FLEET__" {
        return tabs.len().saturating_sub(1);
    }
    let lower = t.to_lowercase();
    if let Ok(n) = lower.parse::<usize>() {
        if n >= 1 && n <= tabs.len() {
            return n - 1;
        }
        eprintln!("ccc: --focus {t}: no tab #{n} (there are {}), opening first tab", tabs.len());
        return 0;
    }
    if let Some(i) = tabs.iter().position(|x| x.eq_ignore_ascii_case(t)) {
        return i;
    }
    if let Some(i) = tabs.iter().position(|x| x.to_lowercase().starts_with(&lower)) {
        return i;
    }
    if let Some(i) = tabs.iter().position(|x| x.to_lowercase().contains(&lower)) {
        return i;
    }
    eprintln!("ccc: --focus '{t}' matches none of [{}], opening first tab", tabs.join(", "));
    0
}

/// `NAME=` on the first word labels the spec: `["-p","2222","user@box"]` minus the label.
/// Operates on pre-split tokens so shell quoting is preserved (review fix #19).
fn split_label(words: &[String]) -> (Option<String>, Vec<String>) {
    let mut words = words.to_vec();
    let first = words.first().cloned().unwrap_or_default();
    // A label must not look like an ssh option token, and must be followed by real args.
    if !first.starts_with('-') {
        if let Some((name, rest)) = first.split_once('=') {
            let label = name.to_string();
            if !rest.is_empty() {
                words[0] = rest.to_string();
            } else {
                words.remove(0);
            }
            return (Some(label), words);
        }
    }
    (None, words)
}

fn default_station() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "LOCAL".to_string());
    format::short_host(&raw)
}

fn shrink_size() -> (u16, u16) {
    // Prefer real terminal size when attached; fixed frame for pipes/CI.
    if let (Ok(w), Ok(h)) = (std::env::var("COLUMNS"), std::env::var("LINES")) {
        if let (Ok(w), Ok(h)) = (w.parse::<u16>(), h.parse::<u16>()) {
            if w >= 30 && h >= 12 {
                return (w.min(300), h.min(200));
            }
        }
    }
    (92, 36)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(|w| w.to_string()).collect()
    }

    #[test]
    fn labeled_specs() {
        assert_eq!(
            split_label(&words("orion=10.0.0.7")),
            (Some("orion".into()), vec!["10.0.0.7".to_string()])
        );
        let (l, t) = split_label(&words("gpu=-p 2222 root@box.local"));
        assert_eq!(l, Some("gpu".into()));
        assert_eq!(t, vec!["-p", "2222", "root@box.local"]);
        let (l, t) = split_label(&words("user@plain-host"));
        assert_eq!(l, None);
        assert_eq!(t, vec!["user@plain-host"]);
    }

    #[test]
    fn spec_label_needs_equals() {
        // Bare "name host" is ambiguous with options; require NAME=host form.
        let (l, t) = split_label(&words("just a bare word"));
        assert_eq!(l, None);
        assert_eq!(t, vec!["just", "a", "bare", "word"]);
    }

    #[test]
    fn focus_resolution() {
        let tabs = ["fedora".to_string(), "rack4".to_string(), "K8S:homelab".to_string()];
        assert_eq!(resolve_focus(None, &tabs), 0);
        assert_eq!(resolve_focus(Some("fedora"), &tabs), 0);
        assert_eq!(resolve_focus(Some("FEDORA"), &tabs), 0);
        assert_eq!(resolve_focus(Some("rack4"), &tabs), 1);
        assert_eq!(resolve_focus(Some("k8s"), &tabs), 2); // case-insensitive prefix
        assert_eq!(resolve_focus(Some("home"), &tabs), 2); // substring
        assert_eq!(resolve_focus(Some("2"), &tabs), 1); // 1-based tab number
        assert_eq!(resolve_focus(Some("__FLEET__"), &tabs), 2);
        // unmatched targets warn and land on tab 0 rather than failing the launch
        assert_eq!(resolve_focus(Some("nosuch"), &tabs), 0);
        assert_eq!(resolve_focus(Some("99"), &tabs), 0);
    }
}
