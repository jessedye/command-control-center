//! End-to-end smoke tests driving the built `ccc` binary — the only checks that exercise
//! argument validation, exit codes, and the rendered frame as a user would see them.

use std::process::Command;

fn ccc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ccc"))
}

#[test]
fn once_renders_a_frame_with_sections() {
    let out = ccc().args(["--once", "--width", "96", "--height", "36", "--no-color"]).output().expect("run");
    assert!(out.status.success(), "exit {}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    for probe in ["COMMAND CONTROL", "CPU ", "MEM ", "NVRAM", "DISK ", "TOP PROCS"] {
        assert!(text.contains(probe), "frame missing {probe:?}: {text}");
    }
    // Footer reports whichever end of the status line applies to THIS machine; both are valid.
    assert!(
        text.contains("ALL STATIONS NOMINAL") || text.contains("ACTIVE ALERTS"),
        "footer missing status summary: {text}"
    );
}

#[test]
fn rejects_nan_interval_and_out_of_range_thresholds() {
    // Review fix #14: NaN used to pass validation and panic inside Duration::from_secs_f64.
    let out = ccc().args(["--interval", "NaN", "--once"]).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "NaN interval must be an argument error");
    let err = String::from_utf8_lossy(&out.stderr);
    // Wording dropped the literal `--` prefix when settings gained config-file sources;
    // the invariant tested here is exit 2 with a named complaint, not the dash.
    assert!(err.contains("interval"), "message: {err}");

    let out = ccc().args(["--warn", "nan", "--once"]).output().expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("warn"), "message: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn rejects_unquoted_malformed_host_spec() {
    // `-p` with no value: ssh token parsing must fail fast with a helpful message.
    let out = ccc().args(["-H", "box=-p", "--once"]).output().expect("run");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("missing value"), "message: {err}");
}

#[test]
fn version_flag_works() {
    let out = ccc().arg("--version").output().expect("run");
    assert!(out.status.success());
    let v = String::from_utf8_lossy(&out.stdout);
    assert!(v.contains("ccc"), "stdout: {v}");
}

#[test]
fn config_file_defines_named_stations_and_focus() {
    let dir = std::env::temp_dir().join(format!("ccc-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let path = dir.join("lab.conf");
    std::fs::write(&path, "local fedora\nk8s default homelab\nfocus fedora\n").expect("write conf");
    let out = ccc()
        .args(["--config", path.to_str().unwrap(), "--once", "--width", "96", "--height", "36", "--no-color"])
        .output()
        .expect("run");
    assert!(out.status.success(), "exit {}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    // focus=fedora => local tab renders its CPU section under label fedora;
    // K8S:homelab appears as a tab in the strip even where kubectl is absent (STALE).
    assert!(text.contains("fedora"), "tab/local label missing: {text}");
    assert!(text.contains("K8S:homelab"), "fleet tab label missing: {text}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn explicit_missing_config_is_fatal() {
    // A typo'd --config path must fail loudly, not silently run with zero stations.
    let out = ccc().args(["--config", "/nonexistent-dir/ccc.conf", "--once"]).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent-dir/ccc.conf"));
}

#[test]
fn station_flags_replace_config_stations() {
    let dir = std::env::temp_dir().join(format!("ccc-cfg-rep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let path = dir.join("lab.conf");
    std::fs::write(&path, "ssh ghostbox ghost.invalid\n").expect("write conf");
    let out = ccc()
        .args(["--config", path.to_str().unwrap(), "-H", "visible=example.invalid", "--once", "--width", "96", "--height", "30", "--no-color"])
        .output()
        .expect("run");
    assert!(out.status.success(), "exit {}\nstderr: {}", out.status, String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    // CLI wins wholesale: the file's station must NOT appear; the -H label must.
    assert!(!text.contains("ghostbox"), "config station leaked past CLI override: {text}");
    assert!(text.contains("visible"), "CLI station label missing: {text}");
    std::fs::remove_dir_all(&dir).ok();
}
