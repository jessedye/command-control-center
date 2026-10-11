//! Proxmox VE collection: one seed-node ssh session yields the whole cluster view.
//!
//! `pvesh get /cluster/status` prints every node's name, IP and online flag (and the
//! quorum/version summary); `/cluster/resources --type node|vm` carries cpu/mem/uptime and
//! the guest inventory. All three run inside ONE bounded `ssh host sh -s` round-trip —
//! no agent, no per-node login, no jq dependency (jq is NOT installed on Proxmox nodes by
//! default, so `--output-format json` fails there with "Parameter verification failed"
//! noise; the default human tables must be parsed instead).
//!
//! Values arrive PRESENTED (percent strings, GiB/TiB units, "4w 2d 11h" uptimes) because
//! pvesh formats through its internal table renderer. That rendering is the stable public
//! surface operators read, so it is what we parse — column lookup is by header NAME, never
//! by position, so PVE reordering fields between releases shifts nothing here.

use crate::collect;
use crate::snapshot::{PveCluster, PveGuest, PveNode, PveStorage};

/// One watched Proxmox cluster: how to reach a seed node (label shown on the tab).
#[derive(Debug, Clone)]
pub struct PveSpec {
    /// See RemoteSpec::retry_ms — applied to the seed connection's throttle retry.
    pub retry_ms: u64,
    pub label: String,
    /// Destination token as given (`root@192.0.2.241`).
    pub host: String,
    /// Extra ssh argv minus the destination (ports, jump hosts), shared with discovery.
    pub ssh_args: Vec<String>,
    /// Password file for sshpass (Proxmox root typically has no authorized key).
    pub pass_file: Option<String>,
}

/// Combined script: one ssh round-trip yields every panel. Sections are `@@TAG`-marked
/// with `2>&1`, so a missing pvesh shows up INSIDE its section (the parser turns that into
/// a readable note instead of an empty table).
///
/// Storage usage is only reported per-node (`/nodes/<n>/storage`) — a CephFS/RBD/NFS export
/// mounted on all five hypervisors gets one row per node that sees it; the renderer collapses
/// by (name,kind) and sums capacity across online nodes, deduped via the shared column.
/// `/etc/pve/nodes` is the FUSE cluster view, so iterating it from the seed covers every
/// node without ssh-hopping; nodes whose FUSE dir is empty (offline) simply produce no rows.
// Raw string: the embedded python uses " freely, and its newlines/indentation are literal
// (a normal Rust literal would need \" escapes and \-continuations flatten the indentation).
pub const PVE_SCRIPT: &str = r#"
echo '@@STATUS'
pvesh get /cluster/status 2>&1
for n in $(ls /etc/pve/nodes 2>/dev/null); do echo "@@STORAGE:$n"; pvesh get /nodes/$n/storage 2>&1; done
echo '@@CEPH'
if command -v ceph >/dev/null 2>&1; then timeout 15 ceph status --format json 2>/dev/null | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin)
except Exception:
    sys.exit(0)
print("overall_status", d["health"]["status"])
for name,c in list(d.get("health",{}).get("checks",{}).items())[:6]:
    s=c.get("summary")
    msg=s.get("message","") if isinstance(s,dict) else "; ".join(str(x) for x in (s or []))
    print("check", name, c.get("severity",""), " ".join(msg.split()))' 2>/dev/null; fi
echo '@@NODES'
pvesh get /cluster/resources --type node 2>&1
echo '@@VM'
pvesh get /cluster/resources --type vm 2>&1
"#;

pub struct PveCollector {
    pub spec: PveSpec,
}

impl PveCollector {
    pub fn new(spec: PveSpec) -> Self {
        Self { spec }
    }

    pub fn sample(&self) -> Result<PveCluster, String> {
        let first = collect::run_remote_script(PVE_SCRIPT, &self.spec.ssh_args, &self.spec.host, self.spec.pass_file.as_deref());
        let text = match first {
            Err(e) if collect::auth_throttled(&e) => {
                // One slow retry, same rationale as RemoteCollector::ssh_run: rapid reconnects
                // extend server-side lockouts; the next tick is the natural second chance.
                std::thread::sleep(std::time::Duration::from_millis(self.spec.retry_ms.max(250)));
                collect::run_remote_script(PVE_SCRIPT, &self.spec.ssh_args, &self.spec.host, self.spec.pass_file.as_deref())?
            }
            Ok(t) => t,
            Err(e) => return Err(e),
        };
        Ok(parse_pve_output(&text))
    }
}

/// Cluster node roster `(name, ip)` from a single seed connection — feeds station discovery.
/// Returns pairs in cluster order; offline nodes are included (ccc surfaces their dead-link
/// frames like any other station) — callers filter by needing a working collector themselves.
pub fn discover_nodes(spec: &PveSpec) -> Result<Vec<(String, String)>, String> {
    let script = "echo '@@STATUS'\npvesh get /cluster/status 2>&1\n";
    let text = collect::run_remote_script(script, &spec.ssh_args, &spec.host, spec.pass_file.as_deref())?;
    let cluster = parse_pve_output(&text);
    Ok(cluster.nodes.into_iter().map(|n| (n.name, n.ip)).collect())
}

// ---------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------

/// Parse the combined multi-section output. Unrecognized text degrades to NOTES, never a
/// hard failure: one unparsable section must not blank the panels that did parse.
pub fn parse_pve_output(text: &str) -> PveCluster {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(tag) = t.strip_prefix("@@") {
            if let Some(sec) = current.take() {
                sections.push(sec);
            }
            current = Some((tag.to_string(), String::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
        // Lines before the first marker: stderr preamble (ssh warnings); ignore.
    }
    if let Some(sec) = current {
        sections.push(sec);
    }

    let mut notes: Vec<String> = Vec::new();
    if sections.is_empty() {
        notes.push("no proxmox sections in output (is this a PVE node?)".to_string());
        return PveCluster { notes, ..Default::default() };
    }

    let mut status_rows = Vec::new();
    let mut node_rows = Vec::new();
    let mut vm_rows = Vec::new();
    // Per-node storage sections keep their node name in the tag ("STORAGE:node3").
    let mut storage: Vec<PveStorage> = Vec::new();
    let mut ceph_raw = String::new();
    for (tag, body) in sections {
        if let Some(node) = tag.strip_prefix("STORAGE:") {
            for r in parse_pvesh_table(&body) {
                // An empty result set (no storages attached to that node) yields header-only
                // tables → zero data rows; the header row never reaches us as a map because
                // it has no numeric 'vmid', so filtering on name+total presence is enough.
                let name = field(&r, "storage");
                if name.is_empty() {
                    continue;
                }
                storage.push(PveStorage {
                    node: node.to_string(),
                    name,
                    kind: field(&r, "type"),
                    content: field(&r, "content"),
                    total_bytes: parse_bytes(cell(&r, "total")),
                    avail_bytes: parse_bytes(cell(&r, "avail")),
                    used_bytes: parse_bytes(cell(&r, "used")),
                    used_pct: parse_pct(cell(&r, "used_fraction")),
                });
            }
            continue;
        }
        let rows = parse_pvesh_table(&body);
        if rows.is_empty() {
            if tag == "CEPH" {
                ceph_raw = body;
                continue; // key/value renderer, not a box table — parsed below
            }
            // Empty table means the command itself failed — report its first line verbatim
            // (pvesh errors, "sh: pvesh: not found", API 500s all land here).
            let why = body.trim().lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            let why = if why.is_empty() { "empty output".to_string() } else { why.to_string() };
            notes.push(format!("{tag}: {why}"));
            continue;
        }
        match tag.as_str() {
            "STATUS" => status_rows = rows,
            "NODES" => node_rows = rows,
            "VM" => vm_rows = rows,
            _ => notes.push(format!("unknown section '{tag}'")),
        }
    }
    // A `--type` filter response containing only the cluster-scoped tables is fine; the raw
    // ceph text is consumed below and dropped from sections.
    let ceph_health = parse_ceph_status(&ceph_raw);

    // ---- cluster roster (STATUS): name/ip/online, plus cluster-level quorum facts ----
    let mut nodes: Vec<PveNode> = Vec::new();
    for r in &status_rows {
        if r.get("type").map(|s| s.as_str()) != Some("node") {
            continue; // "cluster" row handled via notes
        }
        let name = field(r, "name");
        if name.is_empty() {
            continue;
        }
        nodes.push(PveNode {
            name: name.clone(),
            ip: field(r, "ip"),
            online: field(r, "online") == "1",
            cpu_pct: None,
            max_cpu: 0,
            mem_bytes: 0,
            max_mem_bytes: 0,
            uptime_s: 0,
        });
    }
    let cluster_row = status_rows.iter().find(|r| r.get("type").map(|s| s.as_str()) == Some("cluster"));
    if let Some(c) = cluster_row {
        if field(c, "quorate") == "1" {
            notes.push("quorum OK".to_string());
        } else if !field(c, "quorate").is_empty() {
            notes.push("NO QUORUM (corosync)".to_string());
        }
    }

    // ---- merge per-node metrics (NODES table), keyed by node name ----
    // PVE 9 renders resources --type node with the `name` column EMPTY on hypervisor rows;
    // the node name rides in the `node` column instead. Renderers that fill `name` work too.
    // (Live cluster, 2026-10-11: metrics rendered "-" until this fallback was added.)
    for r in &node_rows {
        let name = {
            let n = field(r, "name");
            if n.is_empty() { field(r, "node") } else { n }
        };
        if name.is_empty() {
            continue;
        }
        if let Some(n) = nodes.iter_mut().find(|n| n.name == name) {
            n.cpu_pct = parse_pct(cell(r, "cpu"));
            n.max_cpu = parse_int(cell(r, "maxcpu"));
            n.mem_bytes = parse_bytes(cell(r, "mem"));
            n.max_mem_bytes = parse_bytes(cell(r, "maxmem"));
            n.uptime_s = parse_uptime(cell(r, "uptime"));
        }
    }

    // Health-check details ("check <name> <severity> <summary>") become readable notes so a
    // HEALTH_WARN names the offending subsystem, not just the color.
    for line in ceph_raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("check ") {
            let mut words = rest.splitn(3, char::is_whitespace);
            let (name, sev) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let detail = words.next().unwrap_or("").trim();
            notes.push(if detail.is_empty() { format!("ceph [{sev}] {name}") } else { format!("ceph [{sev}] {detail}") });
        }
    }

    // Unintelligible ceph output (no json, python missing, timeout) must leave a trace; a
    // cluster with no ceph at all prints nothing at all and stays silent on purpose.
    if ceph_health.is_none() && !ceph_raw.trim().is_empty() {
        let why = ceph_raw
            .trim()
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('-') && !l.starts_with("check "))
            .unwrap_or("unreadable output");
        notes.push(format!("ceph: {why}"));
    }

    // ---- guests (VM table) ----
    let mut guests: Vec<PveGuest> = Vec::new();
    for r in &vm_rows {
        let (kind, vmid) = match parse_id(cell(r, "id")) {
            Some(v) => v,
            None => continue,
        };
        guests.push(PveGuest {
            vmid,
            kind,
            name: field(r, "name"),
            node: field(r, "node"),
            status: field(r, "status"),
            template: cell(r, "template") == "1",
            cpu_pct: parse_pct(cell(r, "cpu")),
            max_cpu: parse_int(cell(r, "maxcpu")),
            mem_bytes: parse_bytes(cell(r, "mem")),
            max_mem_bytes: parse_bytes(cell(r, "maxmem")),
        });
    }

    PveCluster { nodes, guests, storage, ceph_health, notes }
}

/// Extract Ceph's health from the collector's `ceph status --format json` translation: the
/// Python shim prints one `overall_status <HEALTH_X>` line plus optional `check …` detail
/// lines. Prefer `overall_status`; fall back to the first bare `status` line (kept for the
/// historical pvesh shape). Returns None when no status line exists (no Ceph on the host →
/// empty output; the Python shim failed → caller notes the raw first line).
pub fn parse_ceph_status(text: &str) -> Option<String> {
    let mut nested: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('-') {
            continue;
        }
        let mut words = t.splitn(2, char::is_whitespace);
        let (k, v) = (words.next()?, words.next().unwrap_or("").trim());
        if v.is_empty() {
            continue;
        }
        if k == "overall_status" {
            return Some(v.to_string());
        }
        if k == "status" && nested.is_none() {
            nested = Some(v.to_string());
        }
    }
    nested
}

// ---------------------------------------------------------------------------
// Table + value parsers
// ---------------------------------------------------------------------------

/// Parse one pvesh ASCII box table (`│ name │ …` with ╞/─ separators) into
/// header-lowercased maps. Non-box output (shell errors) yields zero rows.
pub fn parse_pvesh_table(text: &str) -> Vec<std::collections::HashMap<String, String>> {
    let mut rows: Vec<std::collections::HashMap<String, String>> = Vec::new();
    let mut header: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('│') {
            continue;
        }
        let cells: Vec<&str> = t.trim_matches('│').split('│').map(|c| c.trim()).collect();
        if header.is_empty() {
            header = cells.iter().map(|c| c.to_ascii_lowercase()).collect();
            continue;
        }
        // A data row with a trailing '│' splits into len(header)+1 pieces where the last
        // is the post-bar remainder: trim_matches handles both orientations.
        let mut row = std::collections::HashMap::new();
        for (i, name) in header.iter().enumerate() {
            row.insert(name.clone(), cells.get(i).copied().unwrap_or("").to_string());
        }
        rows.push(row);
    }
    rows
}

fn field(r: &std::collections::HashMap<String, String>, k: &str) -> String {
    r.get(k).cloned().unwrap_or_default()
}

fn cell<'a>(r: &'a std::collections::HashMap<String, String>, k: &str) -> &'a str {
    r.get(k).map(String::as_str).unwrap_or("")
}

/// "23.06%" -> 23.06; "", "-", "N/A" -> None; out-of-range clamps to 0..=100.
pub fn parse_pct(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('%').trim();
    if s.is_empty() || s == "-" {
        return None;
    }
    let v = s.parse::<f64>().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some(v.clamp(0.0, 100.0))
}

/// "4.26 GiB" -> bytes. Binary and SI spellings, case-insensitive, plain integers = bytes.
/// pvesh renders sizes human-scale (KiB/MiB/GiB/TiB), values up to exabytes.
pub fn parse_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() || s == "-" {
        return 0;
    }
    // Split leading number from trailing unit.
    let cut = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(s.len());
    let (num, unit) = (s[..cut].trim(), s[cut..].trim().to_ascii_uppercase());
    let v: f64 = match num.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => v,
        _ => return 0,
    };
    let mult: u64 = match unit.as_str() {
        "" | "B" => 1,
        "KIB" | "K" => 1u64 << 10,
        "MIB" | "M" => 1u64 << 20,
        "GIB" | "G" => 1u64 << 30,
        "TIB" | "T" => 1u64 << 40,
        "PIB" | "P" => 1u64 << 50,
        "EIB" | "E" => 1u64 << 60,
        "KB" => 1000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "PB" => 1_000_000_000_000_000,
        "EB" => 1_000_000_000_000_000_000,
        // Unknown unit: refuse to guess a magnitude; report bytes==0 (dash on screen).
        _ => 0,
    };
    if mult == 0 {
        return 0;
    }
    (v * mult as f64) as u64
}

/// "4w 2d 11h" / "9w 6d 9h" / "0s" / bare seconds -> total seconds. Unparsable -> 0.
pub fn parse_uptime(s: &str) -> u64 {
    let mut total = 0u64;
    let mut chunk = String::new();
    let mut ok_any = false;
    for c in s.trim().chars() {
        if c == ' ' {
            ok_any |= consume_uptime_chunk(&mut chunk, &mut total);
            continue;
        }
        chunk.push(c);
    }
    ok_any |= consume_uptime_chunk(&mut chunk, &mut total);
    if ok_any { total } else { s.trim().parse::<u64>().unwrap_or(0) }
}

fn consume_uptime_chunk(chunk: &mut String, total: &mut u64) -> bool {
    // Take ownership so the trim borrow below can't collide with clearing the buffer.
    let owned = std::mem::take(chunk);
    let t = owned.trim();
    if t.is_empty() {
        return false;
    }
    // Split "3w" into (3, w). No-unit chunks ("0s" handled below) are rejected.
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = (&t[..split], &t[split..]);
    let v: u64 = match num.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mult = match unit {
        "w" => 7 * 86400,
        "d" => 86400,
        "h" => 3600,
        "m" | "min" => 60,
        "s" => 1,
        _ => return false,
    };
    *total += v.saturating_mul(mult);
    true
}

/// "12" (cores) / "100" (vmid) -> integer; anything else 0.
fn parse_int(s: &str) -> u64 {
    s.trim().parse::<u64>().unwrap_or(0)
}

/// "qemu/100" -> ("qemu", 100); "lxc/201" -> ("lxc", 201). Anything else None.
fn parse_id(s: &str) -> Option<(String, u32)> {
    let (kind, rest) = s.split_once('/')?;
    Some((kind.to_string(), rest.trim().parse::<u32>().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim `pvesh get /cluster/status` on PVE 9.0.3, captured 2026-10-11 from
    // node1 (192.0.2.241) — fixture drift here means Proxmox changed its renderer.
    const STATUS_FIXTURE: &str = "\
┌────────────┬─────────┬─────────┬────────────────┬───────┬───────┬────────┬───────┬────────┬─────────┬─────────┐
│ id         │ name    │ type    │ ip             │ level │ local │ nodeid │ nodes │ online │ quorate │ version │
╞════════════╪═════════╪═════════╪════════════════╪═══════╪═══════╪════════╪═══════╪════════╪═════════╪═════════╡
│ cluster    │ cluster │ cluster │                │       │       │        │     5 │        │ 1       │       9 │
├────────────┼─────────┼─────────┼────────────────┼───────┼───────┼────────┼───────┼────────┼─────────┼─────────┤
│ node/node1 │ node1   │ node    │ 192.0.2.241 │       │ 1     │      1 │       │ 1      │         │         │
├────────────┼─────────┼─────────┼────────────────┼───────┼───────┼────────┼───────┼────────┼─────────┼─────────┤
│ node/node2 │ node2   │ node    │ 192.0.2.242 │       │ 0     │      2 │       │ 1      │         │         │
├────────────┼─────────┼─────────┼────────────────┼───────┼───────┼────────┼───────┼────────┼─────────┼─────────┤
│ node/node5 │ node5   │ node    │ 192.0.2.245 │       │ 0     │      5 │       │ 1      │         │         │
└────────────┴─────────┴─────────┴────────────────┴───────┴───────┴────────┴───────┴────────┴─────────┴─────────┘
";

    const NODES_FIXTURE: &str = "\
┌────────────┬──────┬─────────────┬─────────┬────────┬───────────┬──────────┬───────────┬─────────┬───────┬──────┬────────┬───────────┬────────────┬───────────┬─────────┬──────┬───────┬────────┬───────┬────────────┬──────┬────────┬─────────┬──────┬──────────┬───────────┬──────┐
│ id         │ type │ cgroup-mode │ content │ cpu    │ disk      │ diskread │ diskwrite │ hastate │ level │ lock │ maxcpu │ maxdisk   │ maxmem     │ mem       │ memhost │ name │ netin │ netout │ node  │ plugintype │ pool │ status │ storage │ tags │ template │ uptime    │ vmid │
╞════════════╪══════╪═════════════╪═════════╪════════╪═══════════╪══════════╪═══════════╪═════════╪═══════╪══════╪════════╪═══════════╪════════════╪═══════════╪═════════╪══════╪═══════╪════════╪═══════╪════════════╪══════╪════════╪═════════╪══════╪══════════╪═══════════╪══════╡
│ node/node1 │ node │           2 │         │ 17.42% │  4.26 GiB │          │           │ online  │       │      │     12 │ 67.73 GiB │  62.48 GiB │ 39.24 GiB │         │      │       │        │ node1 │            │      │ online │         │      │          │ 4w 2d 11h │      │
├────────────┼──────┼─────────────┼─────────┼────────┼───────────┼──────────┼───────────┼─────────┼───────┼──────┼────────┼───────────┼────────────┼───────────┼─────────┼──────┼───────┼────────┼───────┼────────────╪══════╪════════╪═════════╪══════╪══════════╪═══════════╪══════╡
│ node/node2 │ node │           2 │         │ 10.03% │  4.64 GiB │          │           │ online  │       │      │     12 │ 67.73 GiB │  62.48 GiB │ 35.31 GiB │         │      │       │        │ node2 │            │      │ online │         │      │          │ 9w 6d 10h │      │
└────────────┴──────┴─────────────┴─────────┴────────┴───────────┴──────────┴───────────┴─────────┴───────┴──────┴────────┴───────────┴────────────┴───────────┴─────────┴──────┴───────┴────────┴───────┴────────────┴──────┴────────┴─────────┴──────┴──────────┴───────────┴──────┘
";

    const VM_FIXTURE: &str = "\
┌───────────┬──────┬─────────────┬─────────┬────────┬────────┬────────────┬────────────┬─────────┬───────┬──────┬────────┬───────────┬───────────┬───────────┬──────────┬──────────────────────────────┬───────────┬──────────┬───────┬────────────┬──────┬─────────┬─────────┬──────┬──────────┬────────────┬──────┐
│ id        │ type │ cgroup-mode │ content │ cpu    │ disk   │ diskread   │ diskwrite  │ hastate │ level │ lock │ maxcpu │ maxdisk   │ maxmem    │ mem       │ memhost  │ name                         │ netin     │ netout   │ node  │ plugintype │ pool │ status  │ storage │ tags │ template │ uptime     │ vmid │
╞═══════════╪══════╪═════════════╪═════════╪════════╪════════╪════════════╪════════════╪═════════╪═══════╪══════╪════════╪═══════════╪═══════════╪═══════════╪══════════╪══════════════════════════════╪═══════════╪══════════╪═══════╪════════════╪══════╪═════════╪═════════╪══════╪══════════╪════════════╪══════╡
│ qemu/100  │ qemu │             │         │ 0.00%  │ 0.00 B │ 0.00 B     │ 0.00 B     │ stopped │       │      │ 4      │ 100.00 GiB│ 8.00 GiB  │ 0.00 B    │ 0.00 B   │ template-vm-rhel10           │ 0.00 B    │ 0.00 B   │ node1 │            │      │ stopped │         │      │ 1        │ 0s         │ 100  │
├───────────┼──────┼─────────────┼─────────┼────────┼────────┼────────────┼────────────┼─────────┼───────┼──────┼────────┼───────────┼───────────┼───────────┼──────────┼──────────────────────────────┼───────────┼──────────┼───────┼────────────┼──────┼─────────┼─────────┼──────┼──────────┼────────────┼──────┤
│ qemu/101  │ qemu │             │         │ 29.30% │ 0.00 B │ 119.74 GiB │ 1.96 TiB   │ started │       │      │ 8      │ 500.00 GiB│ 32.00 GiB │ 32.03 GiB │ 32.03 GiB│ k3s-agent-02                   │ 5.21 TiB  │ 1.29 TiB │ node3 │            │      │ running │         │      │ 0        │ 3w 6d 8h   │ 101  │
├───────────┼──────┼─────────────┼─────────┼────────┼────────┼────────────┼────────────┼─────────┼───────┼──────┼────────┼───────────┼───────────┼───────────┼──────────┼──────────────────────────────┼───────────┼──────────┼───────┼────────────┼──────┼─────────┼─────────┼──────┼──────────┼────────────┼──────┤
│ lxc/210   │ lxc  │             │         │ 1.50%  │ 0.00 B │ 1.00 GiB   │ 2.00 GiB   │         │       │      │ 2      │ 16.00 GiB │ 4.00 GiB  │ 1.50 GiB  │ 1.50 GiB │ ct-test                      │ 1.00 GiB  │ 2.00 GiB │ node2 │            │      │ running │         │      │ 0        │ 1d 2h 3m   │ 210  │
└───────────┴──────┴─────────────┴─────────┴────────┴────────┴────────────┴────────────┴─────────┴───────┴──────┴────────┴───────────┴───────────┴───────────┴──────────┴──────────────────────────────┴───────────┴──────────┴───────┴────────────┴──────┴─────────┴─────────┴──────┴──────────┴────────────┴──────┘
";

    // Verbatim `pvesh get /nodes/node1/storage` on PVE 9.0.3, captured 2026-10-11 — the
    // exact column set (content leads; there is no id/vmid column here).
    const STORAGE_FIXTURE: &str = "\
┌───────────────────┬─────────────┬─────────┬────────┬────────────┬─────────┬────────┬────────────┬───────────┬───────────────┐
│ content           │ storage     │ type    │ active │      avail │ enabled │ shared │      total │      used │ used_fraction │
╞═══════════════════╪═════════════╪═════════╪════════╪════════════╪═════════╪════════╪════════════╪═══════════╪═══════════════╡
│ backup,vztmpl,iso │ local       │ dir     │ 1      │  59.99 GiB │ 1       │ 0      │  67.73 GiB │  4.26 GiB │         6.28% │
├───────────────────┼─────────────┼─────────┼────────┼────────────┼─────────┼────────┼────────────┼───────────┼───────────────┤
│ backup,vztmpl,iso │ cephfs      │ cephfs  │ 1      │   1.22 TiB │ 1       │ 1      │   1.26 TiB │ 49.00 GiB │         3.79% │
├───────────────────┼─────────────┼─────────┼────────┼────────────┼─────────┼────────┼────────────┼───────────┼───────────────┤
│ rootdir,images    │ local-lvm   │ lvmthin │ 1      │ 139.25 GiB │ 1       │ 0      │ 141.23 GiB │  1.98 GiB │         1.40% │
├───────────────────┼─────────────┼─────────┼────────┼────────────┼─────────┼────────┼────────────┼───────────┼───────────────┤
│ rootdir,images    │ ceph_pool01 │ rbd     │ 1      │ 933.76 GiB │ 1       │ 1      │   1.93 TiB │  1.02 TiB │        52.82% │
└───────────────────┴─────────────┴─────────┴────────┴────────────┴─────────┴────────┴────────────┴───────────┴───────────────┘
";

    // Historical pvesh indented key/value rendering; kept because parse_ceph_status honors a
    // bare `status` line so the fallback path stays tested.
    const CEPH_OK: &str = "\
----------------------------------------
health
----------------------------------------
checks        []
overall_status OK
";

    fn full_output() -> String {
        format!(
            "echo noise ignored\n@@STATUS\n{STATUS_FIXTURE}@@STORAGE:node1\n{STORAGE_FIXTURE}@@CEPH\n{CEPH_OK}@@NODES\n{NODES_FIXTURE}@@VM\n{VM_FIXTURE}"
        )
    }

    #[test]
    fn parses_live_pve_9_tables() {
        let c = parse_pve_output(&full_output());
        assert_eq!(c.nodes.len(), 3, "node rows only, cluster row filtered: {:?}", c.nodes.iter().map(|n| &n.name).collect::<Vec<_>>());
        let n1 = &c.nodes[0];
        assert_eq!((n1.name.as_str(), n1.ip.as_str()), ("node1", "192.0.2.241"));
        assert!(n1.online);
        // Live PVE 9 rows leave `name` empty; these metric assertions ONLY pass through the
        // `node`-column fallback added after the first live run rendered "-" for every gauge.
        assert_eq!(n1.cpu_pct, Some(17.42), "metrics merge into the STATUS roster by name");
        assert_eq!(n1.max_cpu, 12);
        assert_eq!(n1.mem_bytes, (39.24_f64 * 1024f64.powi(3)) as u64, "GiB→bytes");
        assert_eq!(n1.max_mem_bytes, (62.48_f64 * 1024f64.powi(3)) as u64);
        assert_eq!(n1.uptime_s, 4 * 7 * 86400 + 2 * 86400 + 11 * 3600);
        let n2 = &c.nodes[1];
        assert_eq!((n2.cpu_pct, n2.mem_bytes), (Some(10.03), (35.31_f64 * 1024f64.powi(3)) as u64));
        assert_eq!(n2.uptime_s, 9 * 7 * 86400 + 6 * 86400 + 10 * 3600);
        assert_eq!(c.guests.len(), 3);
        let g = &c.guests[1];
        assert_eq!((g.vmid, g.kind.as_str(), g.name.as_str(), g.node.as_str()), (101, "qemu", "k3s-agent-02", "node3"));
        assert_eq!(g.cpu_pct, Some(29.30));
        assert_eq!(g.status, "running");
        assert!(!g.template);
        assert!(c.guests[0].template, "template flag honored");
        // LXC guests ride in the same --type vm table.
        assert_eq!(c.guests[2].kind, "lxc");
        assert_eq!(c.notes.iter().filter(|n| n.contains("quorum OK")).count(), 1);
        // ---- storage inventory (owner request 2026-10-11: NFS/dirs/ceph with usage) ----
        assert_eq!(c.storage.len(), 4, "{:?}", c.storage.iter().map(|s| &s.name).collect::<Vec<_>>());
        let rbd = c.storage.iter().find(|s| s.name == "ceph_pool01").expect("rbd pool row");
        assert_eq!((rbd.node.as_str(), rbd.kind.as_str()), ("node1", "rbd"));
        assert_eq!(rbd.used_pct, Some(52.82));
        assert_eq!(rbd.total_bytes, (1.93_f64 * 1024f64.powi(4)) as u64);
        assert_eq!(rbd.avail_bytes, (933.76_f64 * 1024f64.powi(3)) as u64);
        // Ceph health summary rides along on clusters that run it.
        assert_eq!(c.ceph_health.as_deref(), Some("OK"));
        // Quorum is reported as an explicit note; nothing else should be noisy.
        assert_eq!(c.notes, vec!["quorum OK".to_string()], "{:?}", c.notes);
    }

    // Verbatim translation of `ceph status --format json` on 192.0.2.241, 2026-10-11 —
    // the python shim's exact stdout lines (overall_status + one check row).
    const CEPH_LIVE: &str = "\
overall_status HEALTH_WARN
check 2026-10-11_zzz_slow_ops HEALTH_WARN 2 OSD(s) experiencing slow operations in BlueStore
";

    #[test]
    fn ceph_live_shape_health_and_check_notes() {
        let c = parse_pve_output(&format!("@@CEPH\n{CEPH_LIVE}"));
        assert_eq!(c.ceph_health.as_deref(), Some("HEALTH_WARN"));
        assert_eq!(c.notes.len(), 1, "{:?}", c.notes);
        assert!(c.notes[0].contains("HEALTH_WARN") && c.notes[0].contains("slow operations"), "{:?}", c.notes);
        // A cluster with no ceph: command absent → section empty → silent, no fake note.
        let n = parse_pve_output("@@CEPH\n");
        assert!(n.ceph_health.is_none() && n.notes.is_empty(), "{:?}", n.notes);
    }

    #[test]
    fn ceph_status_variants() {
        assert_eq!(parse_ceph_status(CEPH_OK).as_deref(), Some("OK"));
        let warn = "health\nchecks\n  OSD_DOWN   1 osd down\nstatus     HEALTH_WARN\n";
        assert_eq!(parse_ceph_status(warn).as_deref(), Some("HEALTH_WARN"));
        let overall_last = "overall_status  WARN\nstatus  OK\n";
        assert_eq!(parse_ceph_status(overall_last).as_deref(), Some("WARN"), "top-level wins over nested");
        assert_eq!(parse_ceph_status("bash: line 1: ceph: command not found\n"), None);
        // No-ceph cluster: API error text → note, never a fake health value.
        let c = parse_pve_output("@@CEPH\nGET failed: no ceph config (500)\n");
        assert!(c.ceph_health.is_none());
        assert_eq!(c.notes.len(), 1, "{:?}", c.notes);
        assert!(c.notes[0].contains("no ceph config"), "{:?}", c.notes);
    }

    #[test]
    fn missing_pvesh_surfaces_as_note_not_silence() {
        let out = "@@STATUS\nsh: 1: pvesh: not found\n@@NODES\nsh: 1: pvesh: not found\n@@VM\nsh: 1: pvesh: not found\n";
        let c = parse_pve_output(out);
        assert!(c.nodes.is_empty() && c.guests.is_empty());
        assert_eq!(c.notes.len(), 3, "{:?}", c.notes);
        assert!(c.notes.iter().all(|n| n.contains("pvesh: not found")), "{:?}", c.notes);
    }

    #[test]
    fn value_units() {
        assert_eq!(parse_pct(" 23.06% "), Some(23.06));
        assert_eq!(parse_pct(""), None);
        assert_eq!(parse_pct("-"), None);
        assert_eq!(parse_pct("150%"), Some(100.0), "clamped for bar math");
        assert_eq!(parse_pct("nan"), None);
        assert_eq!(parse_bytes("4.26 GiB"), (4.26 * 1024f64.powi(3)) as u64);
        assert_eq!(parse_bytes("1.96 TiB"), (1.96 * 1024f64.powi(4)) as u64);
        assert_eq!(parse_bytes("0.00 B"), 0);
        assert_eq!(parse_bytes("garbage"), 0);
        assert_eq!(parse_uptime("9w 6d 9h"), 9 * 7 * 86400 + 6 * 86400 + 9 * 3600);
        assert_eq!(parse_uptime("0s"), 0);
        assert_eq!(parse_uptime("1d 2h 3m"), 86400 + 2 * 3600 + 3 * 60);
        assert_eq!(parse_uptime(""), 0);
        assert_eq!(parse_id("qemu/100"), Some(("qemu".to_string(), 100)));
        assert_eq!(parse_id("lxc/210"), Some(("lxc".to_string(), 210)));
        // node/cluster ids carry no numeric suffix — they are not guests, reject cleanly.
        assert_eq!(parse_id("node/node1"), None);
        assert_eq!(parse_id("junk"), None);
        assert_eq!(parse_id("qemu/abc"), None);
    }
}
