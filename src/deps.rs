// Dependency gate: ccc shells out to real-world tools (ssh, sshpass, kubectl). When a launch
// asks for a feature whose binary is absent, every sample of that tab would fail at runtime
// with a spawn error buried in the panel. Check up front, OFFER TO INSTALL through the detected
// package manager, show download instructions on request, and refuse to start half-broken.
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub bin: &'static str,
    pub why: &'static str,
}

/// The binaries this launch will actually exec. Mirrors the collectors exactly:
/// ssh (collect::run_remote_script), sshpass (password mode wraps ssh), kubectl (kube.rs).
pub fn required_bins(has_remote: bool, password_auth: bool, kubernetes: bool) -> Vec<Dep> {
    let mut v = Vec::new();
    if has_remote {
        v.push(Dep { bin: "ssh", why: "SSH collection for remote stations / Proxmox seeds" });
    }
    // sshpass wraps ssh, so it is only demanded when a remote collection will use it.
    if password_auth && has_remote {
        v.push(Dep {
            bin: "sshpass",
            why: "--pass password auth (feeds the password to ssh non-interactively)",
        });
    }
    if kubernetes {
        v.push(Dep { bin: "kubectl", why: "kubernetes tab (kubectl get nodes/pods)" });
    }
    v
}

fn have_on_path(bin: &str) -> bool {
    // The binary name must be INSIDE the -c script: as a trailing argv (`sh -c 'command -v' X`)
    // X becomes $0 and `command -v` runs with no operands, which exits 0 for every name —
    // a probe written that way reports everything installed (caught by live PATH-stripped smoke).
    // `bin` is one of our own &'static constants, never operator input.
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn missing(deps: &[Dep]) -> Vec<Dep> {
    deps.iter().cloned().filter(|d| !have_on_path(d.bin)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    Dnf,
    Apt,
    Pacman,
    Zypper,
    Apk,
    Brew,
    Unknown,
}

/// First match wins; order matches how specific these managers are to their distros.
pub fn detect_pm(lookup: impl Fn(&str) -> bool) -> Pm {
    for (pm, bin) in [
        (Pm::Dnf, "dnf"),
        (Pm::Apt, "apt-get"),
        (Pm::Pacman, "pacman"),
        (Pm::Zypper, "zypper"),
        (Pm::Apk, "apk"),
        (Pm::Brew, "brew"),
    ] {
        if lookup(bin) {
            return pm;
        }
    }
    Pm::Unknown
}

/// Privilege-drop argv: distro installs need root; skip sudo when we already are root.
pub fn needs_sudo(is_root: bool) -> Option<&'static str> {
    if is_root { None } else { Some("sudo") }
}

/// The install invocation for (pm, bin), WITHOUT any sudo prefix. None = package managers do
/// not ship this tool (kubectl lives upstream, not in distro repos).
pub fn install_args(pm: Pm, bin: &str) -> Option<Vec<String>> {
    let args: &[&str] = match (pm, bin) {
        (Pm::Dnf, "ssh") => &["dnf", "install", "-y", "openssh-clients"],
        (Pm::Apt, "ssh") => &["apt-get", "install", "-y", "openssh-client"],
        (Pm::Pacman, "ssh") => &["pacman", "-S", "--noconfirm", "openssh"],
        (Pm::Zypper, "ssh") => &["zypper", "--non-interactive", "install", "openssh"],
        (Pm::Apk, "ssh") => &["apk", "add", "openssh-client"],
        (Pm::Brew, "ssh") => &["brew", "install", "openssh"],
        // Fedora/RHEL dnf carries the Kubernetes client; other distros don't standardize it.
        (Pm::Dnf, "kubectl") => &["dnf", "install", "-y", "kubernetes-client"],
        // sshpass lives in every one of these repos (EPEL on RHEL-family — see manual_hint).
        (Pm::Dnf, "sshpass") => &["dnf", "install", "-y", "sshpass"],
        (Pm::Apt, "sshpass") => &["apt-get", "install", "-y", "sshpass"],
        (Pm::Pacman, "sshpass") => &["pacman", "-S", "--noconfirm", "sshpass"],
        (Pm::Zypper, "sshpass") => &["zypper", "--non-interactive", "install", "sshpass"],
        (Pm::Apk, "sshpass") => &["apk", "add", "sshpass"],
        (Pm::Brew, "sshpass") => &["brew", "install", "esolitos/ipa/sshpass"],
        // kubectl ships outside distro repos everywhere except dnf's kubernetes-client,
        // and its package name varies too — hand it to the operator instead of guessing.
        _ => return None,
    };
    Some(args.iter().map(|s| s.to_string()).collect())
}

/// Human guidance for the [d]ownload path — what to run when our automated installer can't.
pub fn manual_hint(pm: Pm, bin: &str) -> String {
    match (bin, pm) {
        ("kubectl", _) => "kubectl: install the official build — https://kubernetes.io/docs/tasks/tools/\n\
                           Fedora/RHEL: sudo dnf install kubernetes-client · Debian/Ubuntu: snap install kubectl --classic"
            .to_string(),
        ("ssh", Pm::Dnf) => "sudo dnf install -y openssh-clients".into(),
        ("ssh", Pm::Apt) => "sudo apt-get install -y openssh-client".into(),
        ("ssh", Pm::Pacman) => "sudo pacman -S --noconfirm openssh".into(),
        ("ssh", Pm::Zypper) => "sudo zypper install -y openssh".into(),
        ("ssh", Pm::Apk) => "sudo apk add openssh-client".into(),
        ("ssh", Pm::Brew) => "brew install openssh".into(),
        ("sshpass", Pm::Dnf) => "sudo dnf install -y sshpass   (EPEL on RHEL-family)".into(),
        ("sshpass", Pm::Apt) => "sudo apt-get install -y sshpass".into(),
        ("sshpass", Pm::Pacman) => "sudo pacman -S --noconfirm sshpass".into(),
        ("sshpass", Pm::Zypper) => "sudo zypper install -y sshpass".into(),
        ("sshpass", Pm::Apk) => "sudo apk add sshpass".into(),
        ("sshpass", Pm::Brew) => "brew install esolitos/ipa/sshpass".into(),
        (b, Pm::Unknown) => format!("{b}: install from your OS package manager or project site"),
        (b, _) => format!("install '{b}' with your package manager"),
    }
}

/// Ask about every missing dependency. Returns Err(unresolved bins) — caller exits nonzero.
/// `probe`/`run`/IO are injectable so tests never touch the real PATH or spawn packages.
pub fn ensure(
    deps: &[Dep],
    probe: impl Fn(&str) -> bool,
    run_install: &mut dyn FnMut(&[String]) -> bool,
    mut say: impl FnMut(&str),
    mut ask: impl FnMut() -> String,
) -> Result<(), Vec<String>> {
    let gaps: Vec<Dep> = deps.iter().cloned().filter(|d| !probe(d.bin)).collect();
    if gaps.is_empty() {
        return Ok(());
    }
    let pm = detect_pm(|b| probe(b));
    say(&format!(
        "ccc: {} optional tool(s) missing for this launch:",
        gaps.len()
    ));
    for d in &gaps {
        say(&format!("  • {bin} — {why}", bin = d.bin, why = d.why));
    }
    say(&format!(
        "Package manager detected: {}\n",
        match pm {
            Pm::Unknown => "none (offer download instructions manually)".to_string(),
            pm => format!("{pm:?}"),
        }
    ));
    let mut unresolved = Vec::new();
    for d in &gaps {
        say(&format!(
            "Install '{}' now? [y]es ([{pm}] package) · [d] download instructions · [n] skip > ",
            d.bin,
            pm = match pm {
                Pm::Dnf => "dnf",
                Pm::Apt => "apt",
                Pm::Pacman => "pacman",
                Pm::Zypper => "zypper",
                Pm::Apk => "apk",
                Pm::Brew => "brew",
                Pm::Unknown => "?",
            }
        ));
        let ans = ask();
        let a = ans.trim().to_ascii_lowercase();
        if a == "y" || a == "yes" {
            if let Some(argv) = install_args(pm, d.bin) {
                say(&format!("running: {}\n", argv.join(" ")));
                if run_install(&argv) && probe(d.bin) {
                    say(&format!("{} installed.\n", d.bin));
                    continue;
                }
                say(&format!(
                    "install failed — try manually: {}\n",
                    manual_hint(pm, d.bin)
                ));
            } else {
                say(&format!("no automated install for {}; {}\n", d.bin, manual_hint(pm, d.bin)));
            }
        } else if a == "d" || a == "download" {
            say(&format!("{}\n", manual_hint(pm, d.bin)));
        }
        unresolved.push(d.bin.to_string());
    }
    if unresolved.is_empty() {
        Ok(())
    } else {
        Err(unresolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn fixtures() -> Vec<Dep> {
        required_bins(true, true, true)
    }

    #[test]
    fn required_matrix() {
        assert!(required_bins(false, false, false).is_empty(), "local-only launch asks for nothing");
        assert_eq!(
            required_bins(true, true, true).iter().map(|d| d.bin).collect::<Vec<_>>(),
            vec!["ssh", "sshpass", "kubectl"]
        );
        // password auth without remotes is inert — no ssh, so no sshpass.
        assert_eq!(required_bins(false, true, false).len(), 0);
        assert_eq!(required_bins(true, false, false).iter().map(|d| d.bin).collect::<Vec<_>>(), vec!["ssh"]);
    }

    #[test]
    fn pm_detection_prefers_specific_managers() {
        let all = HashMap::from([("dnf", true), ("apt-get", true), ("brew", true)]);
        assert_eq!(detect_pm(|b| *all.get(b).unwrap_or(&false)), Pm::Dnf);
        assert_eq!(detect_pm(|b| b == "apt-get"), Pm::Apt);
        assert_eq!(detect_pm(|_| false), Pm::Unknown);
    }

    #[test]
    fn install_args_cover_every_known_pair() {
        for pm in [Pm::Dnf, Pm::Apt, Pm::Pacman, Pm::Zypper, Pm::Apk, Pm::Brew] {
            for bin in ["ssh", "sshpass"] {
                let argv = install_args(pm, bin).unwrap_or_else(|| panic!("{pm:?} {bin} must be installable"));
                // apt's executable is `apt-get`; every other manager spells itself lowercase.
                let expected = match pm {
                    Pm::Apt => "apt-get".to_string(),
                    pm => format!("{pm:?}").to_ascii_lowercase(),
                };
                assert_eq!(argv[0], expected, "invokes its own manager");
                assert!(argv.iter().any(|a| a.contains(bin)), "names the package for {bin}: {argv:?}");
            }
        }
        assert_eq!(install_args(Pm::Dnf, "kubectl").map(|v| v.join(" ")).as_deref(), Some("dnf install -y kubernetes-client"));
        assert_eq!(install_args(Pm::Brew, "kubectl"), None, "brew has no standard kubectl formula");
        assert_eq!(install_args(Pm::Unknown, "sshpass"), None, "no manager → manual hints only");
    }

    #[test]
    fn manual_hints_never_empty() {
        for pm in [Pm::Dnf, Pm::Apt, Pm::Pacman, Pm::Zypper, Pm::Apk, Pm::Brew, Pm::Unknown] {
            for bin in ["ssh", "sshpass", "kubectl"] {
                assert!(!manual_hint(pm, bin).trim().is_empty(), "{pm:?} {bin}");
            }
        }
        assert!(manual_hint(Pm::Dnf, "kubectl").contains("kubernetes.io/docs/tasks/tools"));
    }

    /// Shared world state for the `ensure` simulations: presence flags mutated by installs.
    fn world(present: &[&str]) -> Rc<RefCell<HashMap<String, bool>>> {
        let mut m: HashMap<String, bool> = [
            ("ssh", false),
            ("sshpass", false),
            ("kubectl", false),
            ("dnf", false),
        ]
        .into_iter()
        .map(|(k, _)| (k.to_string(), false))
        .collect();
        for p in present {
            m.insert((*p).to_string(), true);
        }
        Rc::new(RefCell::new(m))
    }

    fn probe_of(w: &Rc<RefCell<HashMap<String, bool>>>) -> impl Fn(&str) -> bool {
        let w = Rc::clone(w);
        move |b: &str| w.borrow().get(b).copied().unwrap_or(false)
    }

    #[test]
    fn ensure_installs_on_yes_and_reprobes() {
        let w = world(&["ssh", "dnf"]);
        let answers = RefCell::new(vec!["y".to_string(), "n".to_string()].into_iter());
        let mut run: Box<dyn FnMut(&[String]) -> bool> = {
            let w = Rc::clone(&w);
            Box::new(move |argv: &[String]| -> bool {
                if argv.first().map(String::as_str) == Some("dnf") {
                    // dnf install -y <pkg> marks that package present.
                    if let Some(pkg) = argv.last() {
                        w.borrow_mut().insert(pkg.clone(), true);
                    }
                    return true;
                }
                false
            })
        };
        let mut said = String::new();
        let r = ensure(
            &fixtures(),
            probe_of(&w),
            &mut run,
            |m| said.push_str(m),
            || answers.borrow_mut().next().unwrap_or_default(),
        );
        assert_eq!(r.unwrap_err(), vec!["kubectl".to_string()], "the accepted install resolved, 'n' did not");
        assert!(said.contains("[dnf] package"), "prompt names the detected manager: {said}");
        assert!(!said.contains("openssh"), "present tools are never prompted about");
        assert!(said.contains("sshpass installed"), "success reported after re-probe: {said}");
    }

    #[test]
    fn ensure_declines_to_skip_and_download_report_unresolved() {
        let w = world(&["ssh", "dnf"]);
        // gap order from fixtures(): sshpass first, kubectl second — answer [d]ownload on both.
        let answers = RefCell::new(vec!["d".to_string(), "download".to_string()].into_iter());
        let mut run: Box<dyn FnMut(&[String]) -> bool> = Box::new(|_| false);
        let mut said = String::new();
        let r = ensure(
            &fixtures(),
            probe_of(&w),
            &mut run,
            |m| said.push_str(m),
            || answers.borrow_mut().next().unwrap_or_default(),
        );
        assert_eq!(r.unwrap_err(), vec!["sshpass", "kubectl"], "skipped deps stay unresolved");
        assert!(said.contains("kubernetes.io/docs/tasks/tools"), "download path prints the official site");
        assert!(said.contains("sudo dnf install -y sshpass"), "dnf hint for sshpass: {said}");
    }

    #[test]
    fn all_present_resolves_ok_without_questions() {
        let w = world(&["ssh", "sshpass", "kubectl", "apt-get"]);
        let mut run: Box<dyn FnMut(&[String]) -> bool> =
            Box::new(|_| panic!("must not install anything when nothing is missing"));
        let r = ensure(&fixtures(), probe_of(&w), &mut run, |_| {}, || unreachable!("no prompts"));
        assert!(r.is_ok());
    }

    #[test]
    fn path_probe_actually_resolves_names() {
        // Regression guard for the `sh -c 'command -v' X` quirk: as a trailing argv the name is
        // $0, `command -v` runs argument-less and exits 0 for EVERYTHING. A probe built that way
        // passes all unit tests (mocked probes never call it) yet silently approves every launch.
        assert!(have_on_path("sh"), "a tool that exists must report present");
        assert!(
            !have_on_path("ccc-definitely-not-a-real-binary"),
            "a tool that does not exist must NOT probe present"
        );
    }

    #[test]
    fn missing_filters_by_probe() {
        // exercise the public missing() with the real PATH probe on this machine
        let g = missing(&required_bins(true, true, true));
        for d in &g {
            assert!(!have_on_path(d.bin), "{} reported missing must be off PATH", d.bin);
        }
    }
}

/// Live entrypoint: probes PATH, prompts on an interactive terminal (plain text, before the
/// TUI takes over), runs installs through the detected package manager (sudo-prefixed unless
/// root), and returns Err(list) when the operator skipped something the launch requires.
pub fn check_launch(deps: &[Dep]) -> Result<(), Vec<String>> {
    let gaps = missing(deps);
    if gaps.is_empty() {
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let is_root = Command::new("id").arg("-u").output().map(|o| o.stdout.trim_ascii() == b"0").unwrap_or(false);
    let mut out = std::io::stdout();
    // Boxed as the exact trait object `ensure` wants: closures written with inline annotations
    // bind their argument to one concrete lifetime and will not coerce to `&mut dyn FnMut`.
    let mut run: Box<dyn FnMut(&[String]) -> bool> = {
        let pre = needs_sudo(is_root).into_iter().map(str::to_string).collect::<Vec<_>>();
        Box::new(move |argv: &[String]| -> bool {
            let full: Vec<String> = pre.iter().chain(argv.iter()).cloned().collect();
            Command::new(&full[0])
                .args(&full[1..])
                .status()
                .is_ok_and(|s| s.success())
        })
    };
    if !interactive {
        // Nothing to prompt in a pipe/cron/--once run: report and let the caller fail loudly.
        for d in &gaps {
            let _ = writeln!(out, "ccc: missing dependency '{}' ({})", d.bin, d.why);
            let _ = writeln!(out, "       → {}", manual_hint(detect_pm(have_on_path), d.bin));
        }
        return Err(gaps.iter().map(|d| d.bin.to_string()).collect());
    }
    ensure(
        deps,
        have_on_path,
        &mut run,
        |msg| {
            let _ = out.write_all(msg.as_bytes());
            let _ = out.flush();
        },
        || {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line
        },
    )
}
