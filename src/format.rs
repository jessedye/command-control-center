//! Human-readable number formatting and small pure helpers.
//!
//! Everything here is deterministic and allocation-light; `ccc` renders its whole UI through
//! these functions, so their edge cases are pinned by unit tests.

/// Format a byte count in binary units: `512 B`, `42.1 MiB`, `342.1 GiB`.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut n = bytes as f64;
    for unit in UNITS {
        if n < 1024.0 || unit == "PiB" {
            if unit == "B" {
                return format!("{n:.0} {unit}");
            }
            return format!("{n:.1} {unit}");
        }
        n /= 1024.0;
    }
    format!("{n:.1} EiB")
}

/// Format a transfer rate: `42.1 MiB/s`. Negative rates (counter resets) clamp to zero.
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    let v = if bytes_per_sec.is_finite() && bytes_per_sec > 0.0 {
        bytes_per_sec
    } else {
        0.0
    };
    format!("{}/s", fmt_bytes(v as u64))
}

/// Seconds of uptime as command-console shorthand: `03d 14:22`.
pub fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    format!("{d:02}d {h:02}:{m:02}")
}

/// Process age compactly: `34:05` under an hour, `1:04:22` under a day, else `12d 04:11`.
pub fn fmt_etime(secs: u64) -> String {
    if secs >= 86_400 {
        let d = secs / 86_400;
        let h = (secs % 86_400) / 3_600;
        let m = (secs % 3_600) / 60;
        format!("{d}d {h:02}:{m:02}")
    } else {
        let h = secs / 3_600;
        let m = (secs % 3_600) / 60;
        let s = secs % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

/// Whole seconds since the Unix epoch to UTC `HH:MM:SS` (no external deps).
pub fn utc_hms(epoch_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(epoch_secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Days-since-1970 -> `(year, month, day)` via Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn civil_from_unix(epoch_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

/// Best-effort distro label from `/etc/os-release`, e.g. `Fedora 43`.
pub fn distro_name(os_release_text: &str) -> String {
    // PRETTY_NAME wins over NAME regardless of file order.
    let mut fallback = String::new();
    for line in os_release_text.lines() {
        if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
            return rest.trim_matches('"').to_string();
        }
        if fallback.is_empty() {
            if let Some(rest) = line.strip_prefix("NAME=") {
                fallback = rest.trim_matches('"').to_string();
            }
        }
    }
    fallback
}

/// Shorten a hostname: `orion.lan.example` -> `orion`.
pub fn short_host(h: &str) -> String {
    h.split('.').next().unwrap_or(h).to_string()
}

/// Truncate to at most `max` characters (display-cell agnostic: counts chars).
pub fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(15 * 1024 * 1024 / 10), "1.5 MiB");
        assert_eq!(fmt_bytes(342 * 1024u64.pow(3)), "342.0 GiB");
        assert_eq!(fmt_bytes(4 * 1024u64.pow(4)), "4.0 TiB");
    }

    #[test]
    fn rate_clamps_negative() {
        assert_eq!(fmt_rate(-5.0), "0 B/s");
        assert_eq!(fmt_rate(f64::NAN), "0 B/s");
        assert_eq!(fmt_rate(2048.0), "2.0 KiB/s");
    }

    #[test]
    fn uptime_and_etime() {
        assert_eq!(fmt_uptime(0), "00d 00:00");
        assert_eq!(fmt_uptime(3 * 86400 + 14 * 3600 + 22 * 60), "03d 14:22");
        assert_eq!(fmt_etime(59), "0:00:59");
        assert_eq!(fmt_etime(3723), "1:02:03");
        assert_eq!(fmt_etime(86400 + 4 * 3600 + 11 * 60), "1d 04:11");
    }

    #[test]
    fn utc_known_epochs() {
        assert_eq!(utc_hms(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc_hms(1_735_689_600), "2025-01-01 00:00:00Z");
        // leap-day boundaries and a pre-epoch date
        assert_eq!(utc_hms(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(utc_hms(-86_400), "1969-12-31 00:00:00Z");
    }

    #[test]
    fn distro_prefers_pretty() {
        let t = "NAME=\"Fedora\"\nPRETTY_NAME=\"Fedora 43 (Workstation Edition)\"\n";
        assert_eq!(distro_name(t), "Fedora 43 (Workstation Edition)");
        assert_eq!(distro_name("NAME=\"Arch\""), "Arch");
        assert_eq!(distro_name(""), "");
    }

    #[test]
    fn truncate() {
        assert_eq!(trunc("abc", 5), "abc");
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("日本語テキスト", 4), "日本語…");
    }

    #[test]
    fn short_host_strips_domain() {
        assert_eq!(short_host("node-7.cluster.local"), "node-7");
        assert_eq!(short_host("solo"), "solo");
    }
}
