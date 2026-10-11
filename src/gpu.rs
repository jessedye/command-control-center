//! Local GPU telemetry: NVIDIA via `nvidia-smi`, AMD via amdgpu sysfs.
//! Both probes degrade to "no GPUs" — never an error — so `ccc` runs on machines without one.
//!
//! VRAM is what `ccc` labels **NVRAM** throughout the UI (the owner-facing term for
//! accelerator memory capacity).

use crate::snapshot::{GpuRow, GpuVendor};
use std::fs;
use std::path::{Path, PathBuf};

/// All GPUs visible on this machine (best-effort, in stable order).
pub fn detect_local() -> Vec<GpuRow> {
    let mut gpus = probe_nvidia();
    gpus.extend(probe_amdgpu(Path::new("/sys/class/drm"), Path::new("/sys/class/hwmon")));
    gpus
}

fn probe_nvidia() -> Vec<GpuRow> {
    // Review fix #7: nvidia-smi is known to wedge on driver stalls — bound it at 4 s so a
    // hung probe degrades to "no GPU" instead of freezing the sampler thread.
    let args: Vec<String> = [
        "--query-gpu=index,name,memory.used,memory.total,utilization.gpu,temperature.gpu",
        "--format=csv,noheader,nounits",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match crate::collect::run_bounded(
        "nvidia-smi",
        &args,
        None,
        std::time::Duration::from_secs(4),
    ) {
        Ok((Some(0), out, _)) => parse_nvidia_csv(&String::from_utf8_lossy(&out)),
        _ => Vec::new(),
    }
}

/// Public for unit testing against recorded `nvidia-smi` output.
pub fn parse_nvidia_csv(text: &str) -> Vec<GpuRow> {
    let mut out = Vec::new();
    for line in text.lines() {
        // `index, name..., used, total, util, temp` — name may itself contain commas,
        // so count fields from the RIGHT end.
        let parts: Vec<&str> = line.split(", ").collect();
        if parts.len() < 6 {
            continue;
        }
        let n = parts.len();
        // Fields counted from the right end: index + name are variable length.
        // Round-2 finding 31: Rust's f64 parse accepts "NaN"/"inf", so numeric fields must
        // be checked FINITE (and sane) before they render as healthy-looking gauges.
        // `(value, parsed_ok)` per field — memory cells may legally say [N/A].
        let used = opt_num(parts[n - 4]);
        let total = opt_num(parts[n - 3]);
        let util = opt_num(parts[n - 2]);
        let temp = opt_num(parts[n - 1]);
        // Round-2 finding 31 + unified-memory GPUs (owner request 2026-10-11): `[N/A]`
        // in a memory field is LEGAL and must NOT kill the row — the DGX Spark's GB10
        // reports util/temp/power fine while hiding its unified LPDDR5x behind N/A.
        // A non-numeric UTILIZATION still drops the row (it is the gauge itself).
        if !util.1 || util.0 < 0.0 {
            continue;
        }
        let name = parts[1..n - 4].join(", ");
        out.push(GpuRow {
            vendor: GpuVendor::Nvidia,
            name,
            vram_used_mib: if used.1 { Some(used.0.max(0.0)) } else { None },
            vram_total_mib: if total.1 && total.0 > 0.0 { Some(total.0) } else { None },
            util_pct: util.0.min(100.0),
            temp_c: if temp.1 && temp.0 >= 0.0 { Some(temp.0) } else { None },
        });
    }
    out
}

/// `f64` parse with `[N/A]` / `[Not Supported]` support: `(value, ok)` — `ok=false`
/// covers garbage AND legal non-numeric cells (callers decide which matter).
fn opt_num(s: &str) -> (f64, bool) {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']').trim();
    // "N/A", "Not Supported": strip trailing prose ("N/A" has none; keep parse strict).
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() => (v, true),
        _ => (-1.0, false),
    }
}

fn read_int_file(p: &Path) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse::<u64>().ok()
}

pub fn probe_amdgpu(drm_root: &Path, hwmon_root: &Path) -> Vec<GpuRow> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(drm_root) else { return out };
    let temps = amdgpu_temps(hwmon_root);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // cardN only — skip renderD*, cardN-DP-1 connector nodes, etc.
        let tail = match name.strip_prefix("card") {
            Some(t) => t,
            None => continue,
        };
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let device = entry.path().join("device");
        let Some(row) = read_amdgpu_device(&device, &temps) else { continue };
        out.push(row);
    }
    out
}

/// Map canonicalized `/sys/class/hwmon/hwmonN/device` target -> temperature millidegrees.
fn amdgpu_temps(hwmon_root: &Path) -> Vec<(PathBuf, f64)> {
    let mut temps = Vec::new();
    let Ok(entries) = fs::read_dir(hwmon_root) else { return temps };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(label) = fs::read_to_string(dir.join("name")) else { continue };
        if label.trim() != "amdgpu" {
            continue;
        }
        // Edge devices carry GPU die temperature in temp1_input.
        let Some(milli) = read_int_file(&dir.join("temp1_input")) else { continue };
        if let Ok(canon) = fs::canonicalize(dir.join("device")) {
            temps.push((canon, milli as f64 / 1000.0));
        }
    }
    temps
}

fn read_amdgpu_device(device: &Path, temps: &[(PathBuf, f64)]) -> Option<GpuRow> {
    let total = read_int_file(&device.join("mem_info_vram_total"))?;
    let used = read_int_file(&device.join("mem_info_vram_used")).unwrap_or(0);
    let util = read_int_file(&device.join("gpu_busy_percent")).unwrap_or(0);
    let canon = fs::canonicalize(device).ok()?;
    let temp_c = temps.iter().find(|(p, _)| *p == canon).map(|(_, t)| *t);
    Some(GpuRow {
        vendor: GpuVendor::Amd,
        name: amd_card_name(device),
        vram_used_mib: Some(used as f64 / 1048576.0),
        vram_total_mib: Some((total.max(1)) as f64 / 1048576.0),
        util_pct: util as f64,
        temp_c,
    })
}

fn amd_card_name(device: &Path) -> String {
    if let Ok(uevent) = fs::read_to_string(device.join("uevent")) {
        for line in uevent.lines() {
            if let Some(id) = line.strip_prefix("PCI_ID=") {
                return format!("AMD Radeon [{id}]");
            }
        }
    }
    "AMD Radeon".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_csv_plain() {
        let rows = parse_nvidia_csv(
            "0, NVIDIA GeForce RTX 4090, 15834, 24564, 63, 57\n\
             1, NVIDIA H100 80GB HBM3, 81120, 81559, 12, 41\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(rows[0].vram_used_mib, Some(15834.0));
        assert_eq!(rows[0].temp_c, Some(57.0));
        assert_eq!(rows[1].vendor, GpuVendor::Nvidia);
    }

    #[test]
    fn nvidia_csv_comma_in_name_keeps_fields_aligned() {
        // Hypothetical vendor string with commas: numeric tail must stay pinned to the right.
        let rows = parse_nvidia_csv("0, Model, Pro, 100, 200, 10, 30\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Model, Pro");
        assert_eq!(rows[0].vram_used_mib, Some(100.0));
        assert_eq!(rows[0].vram_total_mib, Some(200.0));
        assert_eq!(rows[0].util_pct, 10.0);
        assert_eq!(rows[0].temp_c, Some(30.0));
    }

    #[test]
    fn nvidia_gb10_unified_memory_keeps_the_row_with_na_vram() {
        // Live DGX Spark output (2026-10-11): util/temperature are real numbers while the
        // integrated LPDDR5x shows [N/A]. The row must survive with vram=None so the NVRAM
        // panel can show utilization/temperature at all.
        let rows = parse_nvidia_csv("0, NVIDIA GB10, [N/A], [N/A], 93, 47\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "NVIDIA GB10");
        assert_eq!(rows[0].util_pct, 93.0);
        assert_eq!(rows[0].temp_c, Some(47.0));
        assert_eq!(rows[0].vram_used_mib, None);
        assert_eq!(rows[0].vram_total_mib, None);
        assert_eq!(rows[0].vram_pct(), 0.0);
        // Numeric memory still wins when present (discrete GPUs unchanged).
        let rows = parse_nvidia_csv("0, NVIDIA GeForce RTX 4090, 6144, 24576, 75, 61\n");
        assert_eq!(rows[0].vram_used_mib, Some(6144.0));
        assert_eq!(rows[0].vram_pct(), 100.0 * 6144.0 / 24576.0);
    }

    #[test]
    fn nvidia_garbage_is_dropped() {
        assert!(parse_nvidia_csv("[N/A], [Not Supported]").is_empty());
        assert!(parse_nvidia_csv("").is_empty());
    }

    #[test]
    fn amdgpu_reads_sysfs_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("card0").join("device");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("mem_info_vram_total"), "17179869184\n").unwrap(); // 16 GiB
        fs::write(dev.join("mem_info_vram_used"), "4294967296\n").unwrap(); // 4 GiB
        fs::write(dev.join("gpu_busy_percent"), "42\n").unwrap();
        fs::write(dev.join("uevent"), "DRIVER=amdgpu\nPCI_ID=1002:744C\n").unwrap();
        let row = read_amdgpu_device(&dev, &[]).unwrap();
        assert_eq!(row.vram_total_mib, Some(16384.0));
        assert_eq!(row.vram_used_mib, Some(4096.0));
        assert_eq!(row.util_pct, 42.0);
        assert!(row.name.contains("1002:744C"));
        assert!(read_amdgpu_device(&dev.join("missing"), &[]).is_none());
    }
}
