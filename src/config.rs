//! Configuration files: named stations so plain `ccc` opens your fleet.
//!
//! One directive per line, quote-aware whitespace (same splitter as `-H` specs). Comments:
//! whole-line (`# ...`) and inline (`k8s default homelab  # lab cluster`). A `#` inside
//! quotes, or immediately attached to a word (`key#1`), stays data. Unknown directives are
//! ERRORS, not warnings — a typo'd `hsot` must never silently mean "no stations", that's
//! how a dashboard ends up showing one lonely tab.
//!
//! ```text
//! local  fedora                       # label for the local machine tab
//! ssh    rack4  -p 2222 admin@10.0.0.4
//! k8s    default  homelab             # kubecontext, shown as tab K8S:homelab
//! interval 3
//! top    20
//! warn   70
//! crit   90
//! sort   mem
//! verbose
//! no-color
//! focus  fedora                       # tab to open on (label, prefix, or number)
//! ```

use std::path::PathBuf;

#[derive(Debug, Default, PartialEq)]
pub struct FileConfig {
    /// Label for the local station (overrides hostname).
    pub local_label: Option<String>,
    /// (tab label, ssh argument tokens, destination host) in file order.
    pub remotes: Vec<(String, Vec<String>, String)>,
    /// kube context to watch; exactly one `k8s` directive is allowed.
    pub kube_ctx: Option<String>,
    /// Display name for the fleet tab (defaults to the context name).
    pub kube_label: Option<String>,
    pub interval: Option<f64>,
    pub top: Option<usize>,
    pub warn: Option<f64>,
    pub crit: Option<f64>,
    /// Raw token: validated against cpu|mem|pid by main (keeps SortArg out of this module).
    pub sort: Option<String>,
    pub verbose: bool,
    pub no_color: bool,
    pub focus: Option<String>,
}

/// Resolve the default config path: `$XDG_CONFIG_HOME/ccc/ccc.conf`, falling back to
/// `$HOME/.config/ccc/ccc.conf`. Returns None if neither variable exists.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("ccc").join("ccc.conf"))
}

const SETTINGS: &[&str] = &["interval", "top", "warn", "crit", "sort", "verbose", "no-color", "focus"];

/// Cut an inline comment from one raw config line, shell-style: a `#` starts a comment only
/// at line start or right after whitespace that is NOT inside quotes. `"a # b"` and `key#1`
/// stay intact, so passwords/paths containing # survive (the owner-surfaced bug: a human
/// writes `k8s default homelab  # lab cluster` and the trailing words must not become arguments).
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut sq = false; // inside '...'
    let mut dq = false; // inside "..."
    let mut at_word_start = true; // previous char was whitespace / line start
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' if !sq => {
                i += 2; // escaped next char (outside single quotes)
                at_word_start = false;
                continue;
            }
            b'\'' if !dq => sq = !sq,
            b'"' if !sq => dq = !dq,
            b'#' if !sq && !dq && at_word_start => return &line[..i],
            _ => {}
        }
        at_word_start = c.is_ascii_whitespace();
        i += 1;
    }
    line
}

fn bad(line_no: usize, msg: String) -> String {
    format!("config line {line_no}: {msg}")
}

/// Parse config text. Ok(config) lists values; Err collects ALL problems with line
/// numbers so fixing one typo doesn't reveal the next one on every run.
pub fn parse(text: &str) -> Result<FileConfig, Vec<String>> {
    let mut cfg = FileConfig::default();
    let mut errs: Vec<String> = Vec::new();

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        // Strip comments first so `k8s default homelab # lab cluster` parses as three tokens.
        let trimmed = strip_inline_comment(raw.trim_start()).trim();
        if trimmed.is_empty() {
            continue;
        }
        let tokens = match crate::collect::shell_split(trimmed) {
            Ok(t) => t,
            Err(e) => {
                errs.push(bad(line_no, format!("quoting error: {e}")));
                continue;
            }
        };
        if tokens.is_empty() {
            continue;
        }
        let dir = tokens[0].as_str();
        let args = &tokens[1..];
        match dir {
            "local" => {
                if cfg.local_label.is_some() {
                    errs.push(bad(line_no, "duplicate 'local' directive".into()));
                    continue;
                }
                if args.len() > 1 {
                    errs.push(bad(line_no, format!("'local' takes an optional label, got {} extra words", args.len())));
                    continue;
                }
                match args.first() {
                    Some(l) => match valid_label(l) {
                        Ok(()) => cfg.local_label = Some(l.clone()),
                        Err(e) => errs.push(bad(line_no, e)),
                    },
                    None => cfg.local_label = Some(String::new()), // present-but-default: marker consumed by main? No: treat as no-op
                }
                // A bare `local` just requests the local tab (which is implicit anyway).
                if cfg.local_label.as_deref() == Some("") {
                    cfg.local_label = None;
                }
            }
            "ssh" => {
                if args.len() < 2 {
                    errs.push(bad(line_no, "'ssh' needs a label and a destination, e.g. 'ssh rack4 -p 2222 admin@host'".into()));
                    continue;
                }
                let label = args[0].clone();
                if let Err(e) = valid_label(&label) {
                    errs.push(bad(line_no, e));
                    continue;
                }
                let rest: Vec<String> = args[1..].to_vec();
                match crate::collect::split_ssh_tokens(&rest) {
                    Ok((_flags, host)) => {
                        if cfg.remotes.iter().any(|(l, _, _)| *l == label) {
                            errs.push(bad(line_no, format!("duplicate station label '{label}'")));
                            continue;
                        }
                        cfg.remotes.push((label, rest, host));
                    }
                    Err(e) => errs.push(bad(line_no, format!("invalid ssh spec: {e}")), ),
                }
            }
            "k8s" => {
                if cfg.kube_ctx.is_some() {
                    errs.push(bad(line_no, "only one 'k8s' line per config — run a second ccc with --config".into()));
                    continue;
                }
                if args.is_empty() {
                    errs.push(bad(line_no, "'k8s' needs a context name (use 'default' for current-context)".into()));
                    continue;
                }
                if args.len() > 2 {
                    errs.push(bad(line_no, format!("'k8s' takes CONTEXT [LABEL], got {} extra words", args.len())));
                    continue;
                }
                cfg.kube_ctx = Some(args[0].clone());
                if let Some(l) = args.get(1) {
                    match valid_label(l) {
                        Ok(()) => cfg.kube_label = Some(l.clone()),
                        Err(e) => {
                            errs.push(bad(line_no, e));
                            cfg.kube_ctx = None;
                        }
                    }
                }
            }
            _ if SETTINGS.contains(&dir) => {
                if let Err(e) = apply_setting(&mut cfg, dir, args, line_no) {
                    errs.push(e);
                }
            }
            _ => errs.push(bad(line_no, format!("unknown directive '{dir}' (expected: local, ssh, k8s, {})", SETTINGS.join(", ")))),
        }
    }

    // Cross-field checks: thresholds must be ordered, labels unique, one fleet.
    if let (Some(w), Some(c)) = (cfg.warn, cfg.crit) {
        if !(0.0..=100.0).contains(&w) {
            errs.push("config: warn must be 0..=100".into());
        }
        if !(0.0..=100.0).contains(&c) {
            errs.push("config: crit must be 0..=100".into());
        }
        if c < w {
            errs.push(format!("config: crit ({c}) must be >= warn ({w})"));
        }
    }
    let mut seen: Vec<String> = Vec::new();
    if let Some(l) = &cfg.local_label {
        seen.push(l.clone());
    }
    for (l, _, _) in &cfg.remotes {
        if seen.iter().any(|s| s == l) {
            errs.push(format!("config: duplicate station label '{l}'"));
        }
        seen.push(l.clone());
    }
    if let Some(l) = &cfg.kube_label {
        if seen.iter().any(|s| s == l) {
            errs.push(format!("config: fleet label '{l}' collides with a station label"));
        }
    }

    if errs.is_empty() {
        Ok(cfg)
    } else {
        Err(errs)
    }
}

fn valid_label(l: &str) -> Result<(), String> {
    if l.is_empty() {
        return Err("empty label".to_string());
    }
    if l.chars().count() > 24 {
        return Err(format!("label '{l}' too long (max 24 cells for the tab bar)"));
    }
    if l.chars().any(char::is_control) {
        return Err(format!("label '{l}' contains control characters"));
    }
    Ok(())
}

fn apply_setting(cfg: &mut FileConfig, dir: &str, args: &[String], line_no: usize) -> Result<(), String> {
    // `args` are the tokens AFTER the directive name, so a one-value setting has len 1.
    let one_value = |name: &str| -> Result<String, String> {
        if args.len() != 1 {
            return Err(bad(line_no, format!("'{name}' takes exactly one value, got {}", args.len())));
        }
        Ok(args[0].clone())
    };
    match dir {
        "interval" => {
            let v = one_value("interval")?.parse::<f64>().map_err(|_| bad(line_no, "interval must be a number".into()))?;
            if !v.is_finite() || !(0.25..=3600.0).contains(&v) {
                return Err(bad(line_no, format!("interval {v} outside 0.25..=3600 seconds")));
            }
            cfg.interval = Some(v);
        }
        "top" => {
            let v = one_value("top")?.parse::<usize>().map_err(|_| bad(line_no, "top must be a whole number".into()))?;
            if !(1..=40).contains(&v) {
                return Err(bad(line_no, format!("top {v} outside 1..=40 rows")));
            }
            cfg.top = Some(v);
        }
        "warn" => {
            let v = one_value("warn")?.parse::<f64>().map_err(|_| bad(line_no, "warn must be a number".into()))?;
            if !v.is_finite() {
                return Err(bad(line_no, "warn must be finite".into()));
            }
            cfg.warn = Some(v);
        }
        "crit" => {
            let v = one_value("crit")?.parse::<f64>().map_err(|_| bad(line_no, "crit must be a number".into()))?;
            if !v.is_finite() {
                return Err(bad(line_no, "crit must be finite".into()));
            }
            cfg.crit = Some(v);
        }
        "sort" => {
            let v = one_value("sort")?;
            if !matches!(v.as_str(), "cpu" | "mem" | "pid") {
                return Err(bad(line_no, format!("sort must be cpu, mem, or pid, got '{v}'")));
            }
            cfg.sort = Some(v);
        }
        "verbose" => {
            if !args.is_empty() {
                return Err(bad(line_no, "'verbose' takes no value".into()));
            }
            cfg.verbose = true;
        }
        "no-color" => {
            if !args.is_empty() {
                return Err(bad(line_no, "'no-color' takes no value".into()));
            }
            cfg.no_color = true;
        }
        "focus" => {
            let v = one_value("focus")?;
            valid_label(&v).map_err(|e| bad(line_no, e))?;
            cfg.focus = Some(v);
        }
        _ => unreachable!("settings dispatch"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_example() {
        let cfg = parse(
            r#"# my fleet
            local fedora
            ssh rack4 -p 2222 admin@10.0.0.4
            k8s default homelab
            interval 3
            top 20
            warn 70
            crit 90
            sort mem
            verbose
            no-color
            focus fedora
            "#,
        )
        .expect("clean");
        assert_eq!(cfg.local_label.as_deref(), Some("fedora"));
        assert_eq!(cfg.remotes.len(), 1);
        assert_eq!(cfg.remotes[0].0, "rack4");
        // Destination keeps its user part — split_ssh_tokens hands back the final token.
        assert_eq!(cfg.remotes[0].2, "admin@10.0.0.4");
        assert_eq!(cfg.remotes[0].1, ["-p", "2222", "admin@10.0.0.4"]);
        assert_eq!(cfg.kube_ctx.as_deref(), Some("default"));
        assert_eq!(cfg.kube_label.as_deref(), Some("homelab"));
        assert_eq!(cfg.interval, Some(3.0));
        assert_eq!(cfg.top, Some(20));
        assert_eq!(cfg.warn, Some(70.0));
        assert_eq!(cfg.crit, Some(90.0));
        assert_eq!(cfg.sort.as_deref(), Some("mem"));
        assert!(cfg.verbose && cfg.no_color);
        assert_eq!(cfg.focus.as_deref(), Some("fedora"));
    }

    #[test]
    fn comments_and_blank_lines_only() {
        let cfg = parse("\n#comment\n   # indented\n").unwrap();
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn inline_comments_are_stripped_outside_quotes() {
        let cfg = parse(
            "k8s default homelab  # lab cluster\n\
             ssh box -i \"pass#word\" host.example   # quoted hash stays data\n",
        )
        .expect("clean");
        assert_eq!(cfg.kube_ctx.as_deref(), Some("default"));
        assert_eq!(cfg.kube_label.as_deref(), Some("homelab"));
        // The '#' inside quotes and mid-word never starts a comment:
        assert_eq!(cfg.remotes[0].1, ["-i", "pass#word", "host.example"]);
    }

    #[test]
    fn hash_attached_to_a_word_is_data_not_comment() {
        // mirrors shell: `local fedora#c` sets label "fedora#c"; `local fedora # c` clears it
        let cfg = parse("local fedora#c\n").unwrap();
        assert_eq!(cfg.local_label.as_deref(), Some("fedora#c"));
    }

    #[test]
    fn ssh_needs_label_and_destination() {
        let e = parse("ssh rack4\n").unwrap_err();
        assert_eq!(e.len(), 1);
        assert!(e[0].starts_with("config line 1:"));
        assert!(e[0].contains("label and a destination"));
    }

    #[test]
    fn unknown_directive_is_an_error_not_a_warning() {
        let e = parse("hsot box1 host\n").unwrap_err();
        assert!(e[0].contains("unknown directive 'hsot'"));
    }

    #[test]
    fn collects_every_problem_at_once() {
        let e = parse("hsot x\ninterval nope\ntop 999\n").unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
    }

    #[test]
    fn two_k8s_lines_are_rejected() {
        let e = parse("k8s a\nk8s b\n").unwrap_err();
        assert!(e[0].contains("only one 'k8s' line"));
    }

    #[test]
    fn threshold_order_enforced() {
        let e = parse("warn 80\ncrit 50\n").unwrap_err();
        assert!(e[0].contains("crit (50) must be >= warn (80)"));
    }

    #[test]
    fn duplicate_station_labels_caught() {
        let e = parse("ssh box a.example\nssh box b.example\n").unwrap_err();
        assert!(e.iter().any(|m| m.contains("duplicate station label 'box'")), "{e:?}");
    }

    #[test]
    fn quoted_values_survive() {
        let cfg = parse(r#"ssh keybox -i "~/.ssh/ed25519 key" labhost"#).unwrap();
        assert_eq!(cfg.remotes[0].1, ["-i", "~/.ssh/ed25519 key", "labhost"]);
    }

    #[test]
    fn control_chars_in_labels_rejected() {
        let e = parse("local ab\u{7}x\n").unwrap_err();
        assert!(e[0].contains("control characters"));
    }

    #[test]
    fn ssh_flags_keep_their_argument() {
        let cfg = parse("ssh gpu -p 2222 root@box.local").unwrap();
        assert_eq!(cfg.remotes[0].2, "root@box.local");
        assert_eq!(cfg.remotes[0].1, ["-p", "2222", "root@box.local"]);
    }
}
