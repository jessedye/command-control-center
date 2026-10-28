//! Decoder for the `CCC/1` line protocol emitted by `assets/collector.sh` over SSH.
//!
//! The protocol is deliberately dumb text: `key=value` lines, one frame terminated by `end`.
//! Unknown keys are ignored so a newer collector on a remote box keeps working against an
//! older `ccc` (forward compatibility), and a truncated stream fails loudly with the raw
//! tail in the error so misdiagnosis is impossible.
//!
//! Frame grammar (all byte counts decimal):
//! ```text
//! CCC/1
//! name=<hostname>
//! ts=<epoch seconds>
//! os=<distro label>
//! kernel=<kernel release>
//! up=<seconds since boot>
//! load=<l1> <l5> <l15>
//! cores=<count>
//! cpu=<aggregate percent, 0..100>
//! cpus=<p0> <p1> ...        # per-core percent, len == cores
//! mem=<total_kb> <avail_kb> <swap_total_kb> <swap_free_kb>
//! disk=<mount>|<fstype>|<total_bytes>|<free_bytes>|<avail_bytes>
//! net=<iface>|<rx_bytes_cumulative>|<tx_bytes_cumulative>   # deltas computed receiver-side
//! gpu=nvidia|<name>|<vram_used_mib|-1>|<vram_total_mib|-1>|<util_pct>|<temp_c|-1>
//! The -1 sentinels mark "driver reports N/A" (integrated GPUs with unified memory).
//! inference=<harness>|<endpoint>|<pid>|<active|-1>|<queued|-1>|<prompt_total|-1>|<generated_total|-1>|<native_tok_s|-1>
//! proc=<pid>|<comm>|<user>|<cpu_pct>|<mem_pct>|<rss_kb>|<elapsed_s>|<cmd...>
//! end
//! ```

use crate::inference::Counter as InferenceCounter;
use crate::snapshot::{DiskRow, GpuRow, GpuVendor, NetRow, ProcRow, Station};

pub const PROTO_MAGIC: &str = "CCC/1";

/// Rate baseline for cumulative network counters, keyed per station.
///
/// Review fixes #12/#13: dt is fractional seconds supplied by the caller (monotonic —
/// integer epoch seconds turned a 250 ms transfer of 250 bytes into 250 B/s instead of
/// 1000 B/s), computation is split from commitment (`peek` is pure; callers `commit` only
/// after a successful decode), and every interface seen on the wire produces a row — brand-new
/// interfaces read 0 B/s while warming up rather than vanishing from the panel.
pub struct NetBaseline {
    prev: Vec<(String, u64, u64)>,
}

impl NetBaseline {
    pub fn new() -> Self {
        Self { prev: Vec::new() }
    }

    /// Rates for `iface_counters` against the last committed sample, over `dt_secs`
    /// (seconds since the previous `commit`). Pure — commit the same slice on success.
    pub fn peek(&self, dt_secs: f64, iface_counters: &[(String, u64, u64)]) -> Vec<NetRow> {
        // A non-positive window has no meaningful rate; report warmup zeros.
        let inv = if dt_secs > 0.0 { 1.0 / dt_secs } else { 0.0 };
        iface_counters
            .iter()
            .map(|(name, rx, tx)| {
                let (drx, dtx) = match self.prev.iter().find(|(n, ..)| n == name) {
                    Some((_, prx, ptx)) => {
                        // Counter wraps/re-creations count backwards: clamp to zero this tick.
                        (rx.saturating_sub(*prx) as f64 * inv, tx.saturating_sub(*ptx) as f64 * inv)
                    }
                    None => (0.0, 0.0),
                };
                NetRow { iface: name.clone(), rx_bps: drx, tx_bps: dtx, link_speed_mbps: None }
            })
            .collect()
    }

    /// Adopt these counters as the baseline for the next interval.
    pub fn commit(&mut self, iface_counters: &[(String, u64, u64)]) {
        self.prev = iface_counters.to_vec();
    }
}

impl Default for NetBaseline {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully validated CCC/1 frame: the station payload plus the exact cumulative
/// interface counters THIS frame carried. Round-2 finding 6: rates must be computed from
/// the counters embedded in the decoded frame (bounded by its magic/`end` framing), never
/// from a second regex pass over raw text — otherwise `net=eth0|BAD|1` poisons the rate
/// baseline (malformed parsed as 0) and `net=` records AFTER `end` leak into the sample.
pub struct Frame {
    pub station: Station,
    pub counters: Vec<(String, u64, u64)>,
    pub inference: Vec<InferenceCounter>,
}

/// Parse a full CCC/1 transcript, validating every field (no defaults-on-garbage). Use
/// `into_station` with rates from a NetBaseline to complete the Station.
pub fn parse(text: &str) -> Result<Frame, String> {
    let mut st = Station {
        name: String::new(),
        ts_epoch: 0,
        os_name: String::new(),
        kernel: String::new(),
        uptime_s: 0,
        load: [0.0; 3],
        core_count: 0,
        cpu_avg_pct: 0.0,
        per_core_pct: Vec::new(),
        mem_total_kb: 0,
        mem_avail_kb: 0,
        swap_total_kb: 0,
        swap_free_kb: 0,
        disks: Vec::new(),
        net: Vec::new(),
        gpus: Vec::new(),
        inference: Vec::new(),
        procs: Vec::new(),
    };
    let mut counters: Vec<(String, u64, u64)> = Vec::new();
    let mut inference: Vec<InferenceCounter> = Vec::new();
    // Review fix #13 — strict frame integrity:
    //   * magic must open the frame; data keys before it are a framing bug, not noise
    //   * `end` closes the frame; trailing bytes after it are ignored (never merged in)
    //   * every number is positionally required, finite, and in range; a malformed row is
    //     REJECTED outright rather than shifted (a dropped middle column silently mislabels
    //     capacity panels — the failure class this protocol must make loud)
    let mut seen_magic = false;
    let mut ended = false;

    for line in text.lines() {
        if ended {
            break;
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line == "end" {
            ended = true;
            continue; // terminator carries no payload, so it has no '=' to split on
        }
        // The bare magic line opens the frame.
        if line == PROTO_MAGIC {
            if seen_magic {
                return Err(format!("duplicate {PROTO_MAGIC} magic (frames must not be concatenated)"));
            }
            seen_magic = true;
            continue;
        }
        // Collector output may interleave shell noise (motd, locale warnings): a line that
        // contains no '=' and isn't the magic is noise; skip it.
        let (key, val) = match line.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let known = matches!(
            key,
            "name" | "ts" | "os" | "kernel" | "up" | "load" | "cores" | "cpu" | "cpus"
                | "mem" | "disk" | "net" | "gpu" | "inference" | "proc"
        );
        if !seen_magic && known {
            return Err(format!("'{key}' field appears before {PROTO_MAGIC} magic"));
        }
        match key {
            "name" => st.name = sanitize(val.trim()),
            "ts" => {
                let v = num_f64(key, val)?;
                st.ts_epoch = v.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
            }
            "os" => st.os_name = sanitize(val.trim()),
            "kernel" => st.kernel = sanitize(val.trim()),
            "up" => st.uptime_s = num_f64(key, val)?.max(0.0) as u64,
            "load" => st.load = fixed_f32_3(key, val)?,
            "cores" => st.core_count = num_f64(key, val)?.clamp(0.0, 4096.0) as usize,
            "cpu" => st.cpu_avg_pct = num_f64(key, val)?.clamp(0.0, 100.0) as f32,
            "cpus" => st.per_core_pct = f32_list(key, val)?,
            "mem" => {
                // Round-2 finding 25: `* 1024` conversions happen in render; a KiB value
                // above u64::MAX/1024 wraps to a plausible number, so it must be refused here.
                let [total, avail, swap_total, swap_free] = fixed_kib_4(key, val)?;
                st.mem_total_kb = total;
                st.mem_avail_kb = avail;
                st.swap_total_kb = swap_total;
                st.swap_free_kb = swap_free;
            }
            "disk" => {
                let f: Vec<&str> = val.split('|').collect();
                if f.len() != 5 {
                    return Err(format!("disk row needs 5 fields, got {}: {val}", f.len()));
                }
                let (total, free, avail) = (
                    num_u64("disk", f[2])?,
                    num_u64("disk", f[3])?,
                    num_u64("disk", f[4])?,
                );
                // A remote emitting free > total would make used-bytes subtraction wrap to a
                // huge capacity (finding 25); reject the impossible relation loudly.
                if free > total || avail > total {
                    return Err(format!(
                        "disk row inconsistent: total={total} free={free} avail={avail}"
                    ));
                }
                st.disks.push(DiskRow {
                    mount: sanitize(&unhex(f[0].trim())),
                    fstype: sanitize(f[1].trim()),
                    device: String::new(), // remote frames carry no device identity
                    total_bytes: total,
                    free_bytes: free,
                    avail_bytes: avail,
                });
            }
            "net" => {
                // Strict three-field grammar inside THIS frame's boundaries only (finding 6):
                // `net=` after `end` is unreachable (the loop breaks at `end`).
                let f: Vec<&str> = val.split('|').collect();
                if f.len() != 3 {
                    return Err(format!("net row needs 3 fields, got {}: {val}", f.len()));
                }
                counters.push((
                    sanitize(&unhex(f[0].trim())),
                    num_u64("net", f[1])?,
                    num_u64("net", f[2])?,
                ));
            }
            "gpu" => {
                let f: Vec<&str> = val.split('|').collect();
                if f.len() != 6 {
                    return Err(format!("gpu row needs 6 fields, got {}: {val}", f.len()));
                }
                let vendor = match f[0].trim().to_ascii_lowercase().as_str() {
                    "nvidia" => GpuVendor::Nvidia,
                    "amdgpu" | "amd" => GpuVendor::Amd,
                    _ => GpuVendor::Other,
                };
                // `-1` is the wire sentinel for "driver says N/A" (integrated GB10-style
                // parts); opt_temp's negative-means-None convention, mirrored.
                let used = opt_vram("gpu", f[2])?;
                let total = opt_vram("gpu", f[3])?;
                st.gpus.push(GpuRow {
                    vendor,
                    name: sanitize(&unhex(f[1].trim())),
                    vram_used_mib: used,
                    vram_total_mib: total,
                    util_pct: num_f64("gpu", f[4])?.clamp(0.0, 100.0),
                    temp_c: opt_temp(f[5]),
                });
            }
            "inference" => {
                let f: Vec<&str> = val.split('|').collect();
                if f.len() != 8 {
                    return Err(format!("inference row needs 8 fields, got {}: {val}", f.len()));
                }
                inference.push(InferenceCounter {
                    harness: sanitize(&unhex(f[0].trim())),
                    endpoint: sanitize(&unhex(f[1].trim())),
                    connect_host: String::new(),
                    pid: num_u64("inference", f[2])?.min(u32::MAX as u64) as u32,
                    active: opt_vram("inference", f[3])?,
                    queued: opt_vram("inference", f[4])?,
                    prompt_total: opt_vram("inference", f[5])?,
                    generated_total: opt_vram("inference", f[6])?,
                    native_tps: opt_vram("inference", f[7])?,
                });
            }
            "proc" => {
                let f: Vec<&str> = val.splitn(8, '|').collect();
                if f.len() != 8 {
                    return Err(format!("proc row needs 8 fields, got {}: {val}", f.len()));
                }
                // Finding 30: every escaped text field goes through unhex, not just argv.
                let cmd = sanitize(&unhex(f[7].trim()));
                let comm = sanitize(&unhex(f[1].trim()));
                st.procs.push(ProcRow {
                    pid: num_u64("proc", f[0])? as u32,
                    user: sanitize(f[2].trim()),
                    cpu_pct: num_f64("proc", f[3])?.clamp(0.0, 1e6) as f32,
                    mem_pct: num_f64("proc", f[4])?.clamp(0.0, 100.0) as f32,
                    rss_kb: num_kib("proc", f[5])?,
                    elapsed_s: num_f64("proc", f[6])?.max(0.0) as u64,
                    cmd: if cmd.is_empty() { comm } else { cmd },
                });
            }
            _ => {} // unknown keys: ignore (forward-compat)
        }
    }

    if !seen_magic {
        return Err(format!("missing {PROTO_MAGIC} magic; raw head: {:?}", trim_head(text, 120)));
    }
    if !ended {
        return Err(format!("truncated stream, missing 'end'; raw tail: {:?}", trim_tail(text, 160)));
    }
    Ok(Frame { station: st, counters, inference })
}

/// Strip ASCII control characters from decoded strings. Remote process ARGV, labels, and
/// interface names are attacker-influenced bytes that flow into terminal output; ESC/BEL
/// injection into an operator's terminal is a real vector (round-2 finding 21). Tab is
/// kept out deliberately — columns are computed from widths, and a tab has width 0.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Parse a byte-count in KiB whose consumer multiplies by 1024 (finding 25).
fn num_kib(field: &str, s: &str) -> Result<u64, String> {
    let v = num_u64(field, s)?;
    if v > u64::MAX / 1024 {
        return Err(format!("{field}: KiB value too large to convert to bytes: {s:?}"));
    }
    Ok(v)
}

/// Parse a mandatory finite float; refuse NaN/inf/garbage with a located message.
fn num_f64(field: &str, s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{field}: not a number: {:?}", trim_head(s, 40)))?;
    if !v.is_finite() {
        return Err(format!("{field}: non-finite number '{v}'"));
    }
    Ok(v)
}

fn num_u64(field: &str, s: &str) -> Result<u64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{field}: not a non-negative integer: {:?}", trim_head(s, 40)))
}

fn fixed_f32_3(field: &str, s: &str) -> Result<[f32; 3], String> {
    let mut v = [0.0f32; 3];
    let mut it = s.split_whitespace();
    for slot in v.iter_mut() {
        let t = it.next().ok_or_else(|| format!("{field}: needs 3 numbers, got fewer: {s:?}"))?;
        let n: f32 = t
            .parse()
            .map_err(|_| format!("{field}: not a number: {t:?}"))?;
        if !n.is_finite() {
            return Err(format!("{field}: non-finite number {n}"));
        }
        *slot = n.max(0.0);
    }
    if it.next().is_some() {
        return Err(format!("{field}: extra trailing fields in {s:?}"));
    }
    Ok(v)
}

fn fixed_kib_4(field: &str, s: &str) -> Result<[u64; 4], String> {
    let mut v = [0u64; 4];
    let mut it = s.split_whitespace();
    for slot in v.iter_mut() {
        let t = it.next().ok_or_else(|| format!("{field}: needs 4 integers, got fewer: {s:?}"))?;
        *slot = num_kib(field, t)?;
    }
    if it.next().is_some() {
        return Err(format!("{field}: extra trailing fields in {s:?}"));
    }
    Ok(v)
}

fn f32_list(field: &str, s: &str) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    for t in s.split_whitespace() {
        let n: f32 = t.parse().map_err(|_| format!("{field}: not a number: {t:?}"))?;
        if !n.is_finite() {
            return Err(format!("{field}: non-finite number {n}"));
        }
        out.push(n.clamp(0.0, 100.0));
    }
    Ok(out)
}

/// `gpu=` memory cell: a finite non-negative MiB number, or the `-1` "driver says N/A"
/// sentinel (integrated GPUs like the DGX Spark's GB10 expose utilization/temperature
/// while unified LPDDR5x reports [N/A). Garbage is an error; -1 is data.
fn opt_vram(field: &str, s: &str) -> Result<Option<f64>, String> {
    let v = num_f64(field, s)?;
    if v < 0.0 {
        return Ok(None);
    }
    Ok(Some(v))
}

fn opt_temp(s: &str) -> Option<f64> {
    let v = s.trim();
    let n = v.parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 { None } else { Some(n) }
}

/// `proc=` carries names/commands verbatim, which can contain '|' when we encode it from
/// argv — the encoder hex-escapes those (`%7C`). Decode `%XX` sequences back.
fn unhex(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn trim_head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn trim_tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().rev().take(n).collect();
    chars.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: full parse, no rate rows (the common test shape).
    fn decode(text: &str) -> Result<Station, String> {
        let mut f = parse(text)?;
        f.station.net = Vec::new();
        Ok(f.station)
    }

    const SAMPLE: &str = "\
Welcome to Fedora 43\n
CCC/1
name=orion07
ts=1728604800
os=Fedora 43 (Workstation Edition)
kernel=7.2.6-100.fc43.x86_64
up=286920
load=1.84 1.62 1.31
cores=16
cpu=63.4
cpus=4.2 12.1 0.0 88.9
mem=98678432 89021876 8388608 8388608
disk=/|ext4|984000000000|640000000000|600000000000
disk=/boot|vfat|1048576000|534773760|534773760
net=eth0|123456789|987654321
gpu=nvidia|RTX 6000 Ada Generation|15834|49140|63|57
gpu=amdgpu|AMD Radeon|1024|16384|31|-1
inference=vLLM|127.0.0.1:8000|2481|2|3|500|150|-1
proc=2481|python3|jesse|63.1|1.2|862412|12345|python3 %2Dm http%2Dserver
end
";

    #[test]
    fn decodes_full_frame_with_noise() {
        let st = decode(SAMPLE).expect("frame parses");
        assert_eq!(st.name, "orion07");
        assert_eq!(st.ts_epoch, 1_728_604_800);
        assert_eq!(st.core_count, 16);
        assert!((st.cpu_avg_pct - 63.4).abs() < 1e-3);
        assert_eq!(st.per_core_pct.len(), 4);
        assert_eq!(st.mem_total_kb, 98_678_432);
        assert_eq!(st.disks.len(), 2);
        assert_eq!(st.disks[0].mount, "/");
        assert_eq!(st.gpus.len(), 2);
        assert_eq!(st.gpus[0].vendor, GpuVendor::Nvidia);
        assert_eq!(st.gpus[1].temp_c, None); // -1 sentinel
        assert_eq!(st.procs.len(), 1);
        assert_eq!(st.procs[0].pid, 2481);
        assert_eq!(st.procs[0].cmd, "python3 -m http-server"); // %XX decoded
    }

    #[test]
    fn frame_carries_its_own_counters() {
        // Round-2 finding 6: counters come from INSIDE the parsed frame, typed strictly.
        let f = parse(SAMPLE).expect("frame parses");
        assert_eq!(f.counters, vec![("eth0".to_string(), 123_456_789u64, 987_654_321u64)]);
        assert_eq!(f.inference.len(), 1);
        assert_eq!(f.inference[0].harness, "vLLM");
        assert_eq!(f.inference[0].endpoint, "127.0.0.1:8000");
        assert_eq!(f.inference[0].active, Some(2.0));
        assert_eq!(f.inference[0].queued, Some(3.0));
        assert_eq!(f.inference[0].prompt_total, Some(500.0));
        assert_eq!(f.inference[0].generated_total, Some(150.0));
        assert_eq!(f.inference[0].native_tps, None);
    }

    #[test]
    fn rejects_missing_magic_and_truncation() {
        assert!(decode("name=x\nend\n").is_err());
        assert!(decode("CCC/1\nname=x\n").is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = "CCC/1\nname=a\nfuture_key=whatever\nend\n";
        assert_eq!(decode(text).unwrap().name, "a");
    }

    #[test]
    fn net_rates_delta_over_baseline() {
        let mut b = NetBaseline::new();
        // First sample establishes the baseline: no rate available yet (warmup rows exist).
        let c1 = [("eth0".to_string(), 1000u64, 500u64)];
        let r1 = b.peek(0.0, &c1);
        assert_eq!(r1.len(), 1);
        assert!(r1.iter().all(|r| r.rx_bps == 0.0));
        b.commit(&c1);
        // Second sample 5s later: 5000 rx bytes/5s = 1000 B/s. dt is fractional (fix #12):
        // 250 bytes over 0.25 s must read 1000 B/s, not 250 B/s.
        let c2 = [("eth0".to_string(), 6000u64, 1000u64)];
        let r2 = b.peek(5.0, &c2);
        assert_eq!(r2[0].rx_bps, 1000.0);
        assert_eq!(r2[0].tx_bps, 100.0);
        let c3 = [("eth0".to_string(), 1250u64, 1000u64)];
        assert_eq!(b.peek(0.25, &c3)[0].rx_bps, 1000.0);
        b.commit(&c3);
        // Counter reset (reboot/re-create) clamps to zero instead of negative rates.
        let c4 = [("eth0".to_string(), 10u64, 10u64)];
        assert_eq!(b.peek(1.0, &c4)[0].rx_bps, 0.0);
    }

    #[test]
    fn strict_framing_rejects_corruption() {
        // Data before magic is a framing bug, not shell noise (fix #13).
        assert!(decode("name=x\nCCC/1\nend\n").is_err());
        // Trailing junk after `end` never merges into the frame — including net= records,
        // which used to be swept up by a second pass over raw text (finding 6).
        let f = parse("CCC/1\nname=a\nend\nnet=evil|1|1\nend\n").unwrap();
        assert_eq!(f.station.name, "a");
        assert!(f.counters.is_empty(), "post-end net rows must not reach the baseline");
        // A hole inside mem= shifts columns silently; it must be a loud error instead.
        assert!(decode("CCC/1\nmem=100 BAD 20 10\nend\n").is_err());
        // Malformed net counter values poison baselines as 0; reject at grammar level.
        assert!(parse("CCC/1\nnet=eth0|BAD|1\nend\n").is_err());
        assert!(parse("CCC/1\ninference=vLLM|127.0.0.1:8000|1|2|-1|10|BAD|-1\nend\n").is_err());
        assert!(parse("CCC/1\ninference=vLLM|short\nend\n").is_err());
        // Non-finite numbers are rejected (Rust parses "inf"/"NaN" happily).
        assert!(decode("CCC/1\nname=a\ncpu=inf\nend\n").is_err());
        assert!(decode("CCC/1\nname=a\nload=1 nan 3\nend\n").is_err());
        // Disk rows carry exactly five fields, and impossible size relations fail.
        assert!(decode("CCC/1\ndisk=/|ext4|100|50\nend\n").is_err());
        assert!(decode("CCC/1\ndisk=/|ext4|100|900|50\nend\n").is_err());
        // KiB values that would wrap on *1024 are refused (finding 25).
        assert!(decode("CCC/1\nname=a\nmem=18014398509481984 0 0 0\nend\n").is_err());
        // Control characters are stripped from labels at the protocol boundary (finding 21).
        let st = decode("CCC/1\nname=a\u{1b}[31mRED\u{7}\nend\n").unwrap();
        assert_eq!(st.name, "a[31mRED");
    }
}
