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
        let nums: Vec<f64> =
            [n - 4, n - 3, n - 2, n - 1].iter().filter_map(|&i| parts[i].trim().parse().ok()).collect();
        if nums.len() != 4 {
            continue;
        }
        let (used, total, util, temp) = (nums[0], nums[1], nums[2], nums[3]);
        if !(used.is_finite() && total.is_finite() && util.is_finite() && temp.is_finite()) {
            continue;
        }
        if used < 0.0 || total <= 0.0 || util < 0.0 {
            continue;
        }
        let name = parts[1..n - 4].join(", ");
        out.push(GpuRow {
            vendor: GpuVendor::Nvidia,
            name,
            vram_used_mib: used,
            vram_total_mib: total,
            util_pct: util,
            temp_c: if temp < 0.0 { None } else { Some(temp) },
        });
    }
    out
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
        vram_used_mib: used as f64 / 1048576.0,
        vram_total_mib: (total.max(1)) as f64 / 1048576.0,
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
        assert_eq!(rows[0].vram_used_mib, 15834.0);
        assert_eq!(rows[0].temp_c, Some(57.0));
        assert_eq!(rows[1].vendor, GpuVendor::Nvidia);
    }

    #[test]
    fn nvidia_csv_comma_in_name_keeps_fields_aligned() {
        // Hypothetical vendor string with commas: numeric tail must stay pinned to the right.
        let rows = parse_nvidia_csv("0, Model, Pro, 100, 200, 10, 30\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Model, Pro");
        assert_eq!(rows[0].vram_used_mib, 100.0);
        assert_eq!(rows[0].vram_total_mib, 200.0);
        assert_eq!(rows[0].util_pct, 10.0);
        assert_eq!(rows[0].temp_c, Some(30.0));
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
        assert_eq!(row.vram_total_mib, 16384.0);
        assert_eq!(row.vram_used_mib, 4096.0);
        assert_eq!(row.util_pct, 42.0);
        assert!(row.name.contains("1002:744C"));
        assert!(read_amdgpu_device(&dev.join("missing"), &[]).is_none());
    }
}
