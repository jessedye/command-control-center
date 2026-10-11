//! Kubernetes fleet panel built from plain `kubectl` table output — no API client,
//! no extra dependency. `get nodes` gives identity/status; `top nodes` (metrics-server)
//! adds capacity percentages. If metrics-server is missing the fleet row shows status
//! only and notes why.

use crate::snapshot::{KubeNode, KubePvc, KubePod};
use std::collections::HashMap;
use std::time::Duration;

pub struct KubeCollector {
    /// `Some(ctx)` selects a kubeconfig context; `Some("")`/`None` mean "current context".
    /// Review fix #8: a bare `--k8s` used to pass the literal string "current" to kubectl,
    /// which fails on every kubeconfig without a context by that exact name.
    pub context: Option<String>,
}

/// One fleet poll: node rows plus human-readable notes (e.g. why `top` has no data).
#[derive(Default)]
pub struct KubeFleet {
    pub nodes: Vec<KubeNode>,
    pub notes: Vec<String>,
    /// Container usage from `kubectl top pods --containers` (empty when unavailable).
    pub pods: Vec<KubePod>,
    /// Claims from `kubectl get pvc -A` (empty when unavailable).
    pub pvcs: Vec<KubePvc>,
}

const KUBECTL_TIMEOUT: Duration = Duration::from_secs(20);

/// Custom-column projection with a stable, documented contract (field paths pinned to the
/// API, immune to kubectl default-column drift). Deliberately NOT `.status.capacity`: bound
/// claims report allocated size; pending ones correctly yield <none>.
const PVC_COLUMNS: &str = "NS:.metadata.namespace,NAME:.metadata.name,PHASE:.status.phase,CAP:.status.capacity.storage,CLASS:.spec.storageClassName,VOL:.spec.volumeName";

impl KubeCollector {
    pub fn sample(&self) -> Result<KubeFleet, String> {
        let ctx = effective_context(self.context.as_deref());
        let get = kubectl(&["get", "nodes"], ctx)?;
        // `top` failing (no metrics-server, RBAC denial, …) must not sink the whole tab;
        // review fix #23: KEEP the real reason so permission errors aren't mislabeled as
        // a missing metrics-server.
        let mut notes = Vec::new();
        let top = match kubectl(&["top", "nodes"], ctx) {
            Ok(t) => t,
            Err(e) => {
                notes.push(e);
                String::new()
            }
        };
        let nodes = parse_fleet(&get, &top)?;

        // Pod metrics and PVC listings are OPTIONAL extras: a cluster lacking pod-metrics
        // RBAC still gets its node table. Run them concurrently — four serial 20s-bounded
        // calls would stretch one poll past the sampling interval on slow links.
        let (pods_out, pvcs_out) = std::thread::scope(|s| {
            let tp = s.spawn(|| kubectl(&["top", "pods", "-A", "--containers"], ctx));
            let tv = s.spawn(|| {
                kubectl(&["get", "pvc", "-A", "--no-headers", "-o", &format!("custom-columns={PVC_COLUMNS}")], ctx)
            });
            (tp.join().unwrap_or_else(|_| Err("pod metrics thread panicked".into())), tv.join().unwrap_or_else(|_| Err("pvc thread panicked".into())))
        });
        let pods = match pods_out {
            Ok(t) => parse_top_pods(&t),
            Err(e) => {
                notes.push(format!("pods: {e}"));
                Vec::new()
            }
        };
        let pvcs = match pvcs_out {
            Ok(t) => parse_pvcs(&t),
            Err(e) => {
                notes.push(format!("pvcs: {e}"));
                Vec::new()
            }
        };
        Ok(KubeFleet { nodes, notes, pods, pvcs })
    }
}

/// Resolve the context argument: None/empty = current context (omit --context entirely).
fn effective_context(context: Option<&str>) -> Option<&str> {
    match context {
        Some(c) if !c.is_empty() && c != "current" => Some(c),
        _ => None,
    }
}

fn kubectl(args: &[&str], context: Option<&str>) -> Result<String, String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(ctx) = context {
        argv.push("--context".to_string());
        argv.push(ctx.to_string());
    }
    argv.extend(args.iter().map(|s| s.to_string()));
    // Review fix #7: bound the invocation; a cluster that swallows requests must not
    // occupy a sampler thread indefinitely.
    let (code, stdout, stderr) =
        crate::collect::run_bounded("kubectl", &argv, None, KUBECTL_TIMEOUT)?;
    match code {
        Some(0) => Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Some(c) => {
            let err = String::from_utf8_lossy(&stderr);
            Err(format!(
                "kubectl {} exited {}: {}",
                args.join(" "),
                c,
                crate::format::trunc(err.trim(), 160)
            ))
        }
        None => Err(format!("kubectl {} killed (timeout)", args.join(" "))),
    }
}

/// Parse `kubectl get nodes` default output using header-name lookup (column order is
/// version-dependent; field *names* are stable contract).
pub fn parse_get_nodes(text: &str) -> Vec<(String, String, String, String, String)> {
    let mut rows = Vec::new();
    let mut cols: HashMap<&str, usize> = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if i == 0 {
            for (idx, tok) in tokens.iter().enumerate() {
                cols.insert(tok, idx);
            }
            continue;
        }
        let get = |name: &str| -> String {
            cols.get(name).and_then(|ix| tokens.get(*ix)).copied().unwrap_or("-").to_string()
        };
        rows.push((get("NAME"), get("STATUS"), get("ROLES"), get("AGE"), get("VERSION")));
    }
    rows
}

/// Parse `kubectl top nodes`. Real header: `Name CPU(cores) CPU% MEMORY(bytes) MEMORY%`
/// — matched case-insensitively by prefix because capitalization has drifted across
/// kubectl versions.
pub fn parse_top_nodes(text: &str) -> HashMap<String, (Option<u64>, Option<f32>, Option<u64>, Option<f32>)> {
    let mut out = HashMap::new();
    let mut cols = TopCols::default();
    let mut seen_header = false;
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if !seen_header {
            seen_header = true;
            cols = TopCols::from_tokens(&tokens);
            continue;
        }
        let cell = |ix: Option<usize>| -> &str {
            ix.and_then(|i| tokens.get(i)).copied().unwrap_or("<unknown>")
        };
        out.insert(
            tokens[0].to_string(),
            (
                parse_k8s_cpu_millis(cell(cols.cpu)),
                parse_percent(cell(cols.cpu_pct)),
                parse_k8s_quantity(cell(cols.mem)),
                parse_percent(cell(cols.mem_pct)),
            ),
        );
    }
    out
}

#[derive(Default)]
struct TopCols {
    cpu: Option<usize>,
    cpu_pct: Option<usize>,
    mem: Option<usize>,
    mem_pct: Option<usize>,
}

impl TopCols {
    fn from_tokens(tokens: &[&str]) -> Self {
        let find = |prefix: &str| {
            tokens
                .iter()
                .position(|t| t.to_ascii_uppercase().starts_with(prefix))
        };
        // Order matters: "CPU%" must be claimed before the bare "CPU(" rate column.
        Self {
            cpu: find("CPU("),
            cpu_pct: find("CPU%"),
            mem: find("MEMORY("),
            mem_pct: find("MEM%").or_else(|| find("MEMORY%")),
        }
    }
}

pub fn parse_percent(t: &str) -> Option<f32> {
    let t = t.trim_end_matches('%');
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    t.parse::<f32>().ok()
}

/// `23m` -> 23, `1200m` -> 1200, `2` -> 2000 millicores; `<unknown>` -> None.
pub fn parse_k8s_cpu_millis(t: &str) -> Option<u64> {
    let t = t.trim();
    if let Some(m) = t.strip_suffix('m') {
        return m.parse::<f64>().ok().map(|v| v.round() as u64);
    }
    if t.contains('.') {
        return t.parse::<f64>().ok().map(|v| (v * 1000.0).round() as u64);
    }
    if t == "<unknown>" {
        return None;
    }
    t.parse::<u64>().ok().map(|v| v * 1000)
}

/// Kubernetes quantity suffixes to bytes: Ki/Mi/Gi/Ti (+ decimal k/M/G/T and raw).
pub fn parse_k8s_quantity(t: &str) -> Option<u64> {
    let t = t.trim();
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    const SUFFIX: [(&str, u64); 9] = [
        ("Ki", 1 << 10),
        ("Mi", 1 << 20),
        ("Gi", 1 << 30),
        ("Ti", 1 << 40),
        ("Pi", 1 << 50),
        ("k", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
    ];
    for (suf, mul) in SUFFIX {
        if let Some(num) = t.strip_suffix(suf) {
            return num.parse::<f64>().ok().map(|v| (v * mul as f64) as u64);
        }
    }
    t.parse::<f64>().ok().map(|v| v as u64)
}

/// Parse `kubectl top pods -A --containers` output by header name. Real header:
/// `NAMESPACE  POD  CONTAINER  CPU(cores)  MEMORY(bytes)` — "POD" is labeled "NAME" in
/// kubectl ≤ 1.28, so both names are accepted. Rows without a parsable metric (the
/// "No resources found" chatter, `<unknown>` containers) are skipped, never faked.
pub fn parse_top_pods(text: &str) -> Vec<KubePod> {
    let mut out = Vec::new();
    let mut cols = PodCols::default();
    let mut seen_header = false;
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if !seen_header {
            seen_header = true;
            cols = PodCols::from_tokens(&tokens);
            continue;
        }
        let cell = |ix: Option<usize>| -> Option<&str> { ix.and_then(|i| tokens.get(i)).copied() };
        let (Some(ns), Some(pod)) = (cell(cols.ns), cell(cols.pod)) else { continue };
        let container = cell(cols.container).unwrap_or("-").to_string();
        let (Some(cpu_s), Some(mem_s)) = (cell(cols.cpu), cell(cols.mem)) else { continue };
        let (Some(milli), Some(mem)) = (parse_k8s_cpu_millis(cpu_s), parse_k8s_quantity(mem_s)) else { continue };
        out.push(KubePod {
            namespace: ns.to_string(),
            pod: pod.to_string(),
            container,
            cpu_milli: milli,
            mem_bytes: mem,
        });
    }
    out
}

#[derive(Default)]
struct PodCols {
    ns: Option<usize>,
    pod: Option<usize>,
    container: Option<usize>,
    cpu: Option<usize>,
    mem: Option<usize>,
}

impl PodCols {
    fn from_tokens(tokens: &[&str]) -> Self {
        let find = |names: &[&str]| tokens.iter().position(|t| names.iter().any(|n| t.eq_ignore_ascii_case(n)));
        // Rate columns match by prefix like TopCols: "CPU(cores)" not bare "CPU".
        let prefix = |p: &str| tokens.iter().position(|t| t.to_ascii_uppercase().starts_with(p));
        // Header-name archaeology: modern kubectl (verified against v1.36 k3s, 2026-10-11)
        // prints `NAMESPACE POD NAME CPU(cores) MEMORY(bytes)` — the CONTAINER column is
        // labeled NAME. Older releases print `NAMESPACE NAME ...` where NAME is the POD.
        // A bare NAME header can mean only one of them: POD wins it when no POD column
        // exists; CONTAINER comes from its own name, else the leftover NAME.
        let pod_by_name = find(&["POD"]);
        let name_col = find(&["NAME"]);
        Self {
            ns: find(&["NAMESPACE"]),
            pod: pod_by_name.or(name_col),
            container: if pod_by_name.is_some() {
                name_col.or_else(|| find(&["CONTAINER"]))
            } else {
                find(&["CONTAINER"])
            },
            cpu: prefix("CPU("),
            mem: prefix("MEMORY("),
        }
    }
}

/// Parse the PVC custom-columns projection (see PVC_COLUMNS): whitespace columns in fixed
/// order NS NAME PHASE CAP CLASS VOL; "<none>" becomes None/"-". Short/malformed lines skip.
pub fn parse_pvcs(text: &str) -> Vec<KubePvc> {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|t| t.len() >= 6)
        .map(|t| KubePvc {
            namespace: t[0].to_string(),
            name: t[1].to_string(),
            phase: clean_cell(t[2]),
            capacity_bytes: if t[3].starts_with('<') { None } else { parse_k8s_quantity(t[3]) },
            class: clean_cell(t[4]),
            volume: clean_cell(t[5]),
        })
        .collect()
}

fn clean_cell(t: &str) -> String {
    if t == "<none>" {
        "-".to_string()
    } else {
        t.to_string()
    }
}

fn parse_fleet(get_text: &str, top_text: &str) -> Result<Vec<KubeNode>, String> {
    let base = parse_get_nodes(get_text);
    if base.is_empty() {
        return Err("kubectl returned no node rows".to_string());
    }
    let tops = parse_top_nodes(top_text);
    Ok(base
        .into_iter()
        .map(|(name, status, roles, age, version)| {
            let (cpu_m, cpu_p, mem_b, mem_p) =
                tops.get(&name).cloned().unwrap_or((None, None, None, None));
            KubeNode {
                name,
                status,
                roles,
                age,
                version,
                cpu_millis: cpu_m,
                cpu_pct: cpu_p,
                mem_bytes: mem_b,
                mem_pct: mem_p,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GET: &str = "NAME       STATUS   ROLES           AGE   VERSION\n\
node-1     Ready    control-plane   210d  v1.31.4\n\
node-2     Ready    <none>          34d   v1.31.4\n";

    const TOP: &str = "Name       CPU(cores)   CPU%   MEMORY(bytes)   MEMORY%\n\
node-1     234m         1%     2145Mi          13%\n\
node-2     51m          0%     998640Ki        6%\n";

    #[test]
    fn get_nodes_by_header() {
        let rows = parse_get_nodes(GET);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "node-1");
        assert_eq!(rows[0].2, "control-plane");
        assert_eq!(rows[1].2, "<none>");
    }

    #[test]
    fn top_nodes_merges_by_name() {
        let t = parse_top_nodes(TOP);
        let (cpu_m, cpu_p, mem_b, mem_p) = t.get("node-1").unwrap().clone();
        assert_eq!(cpu_m, Some(234));
        assert_eq!(cpu_p, Some(1.0));
        assert_eq!(mem_b, Some(2145 * 1024 * 1024));
        assert_eq!(mem_p, Some(13.0));
    }

    #[test]
    fn fleet_join_handles_missing_metrics() {
        let fleet = parse_fleet(GET, "").unwrap();
        assert_eq!(fleet.len(), 2);
        assert_eq!(fleet[0].cpu_millis, None);
        assert_eq!(fleet[1].status, "Ready");
    }

    #[test]
    fn quantities() {
        assert_eq!(parse_k8s_cpu_millis("234m"), Some(234));
        assert_eq!(parse_k8s_cpu_millis("2"), Some(2000));
        assert_eq!(parse_k8s_cpu_millis("1.5"), Some(1500));
        assert_eq!(parse_k8s_cpu_millis("<unknown>"), None);
        assert_eq!(parse_k8s_quantity("998640Ki"), Some(998640 * 1024));
        assert_eq!(parse_k8s_quantity("2Gi"), Some(2 << 30));
        assert_eq!(parse_k8s_quantity("1.5Gi"), Some((1.5 * (1u64 << 30) as f64) as u64));
        assert_eq!(parse_percent("13%"), Some(13.0));
        assert_eq!(parse_percent("<unknown>"), None);
    }

    const TOP_PODS: &str = "NAMESPACE   POD              CONTAINER   CPU(cores)   MEMORY(bytes)\n\
         default     web-7f9c-abc     app         12m          88Mi\n\
         default     web-7f9c-abc     sidecar     3m           21Mi\n\
         kube-system coredns-5d77     coredns     1500m        1.5Gi\n\
         monitoring  node-x-q1        exporter    <unknown>    44Mi\n\
         No resources found.\n";

    #[test]
    fn top_pods_parses_by_header_and_skips_unparsable_rows() {
        let rows = parse_top_pods(TOP_PODS);
        assert_eq!(rows.len(), 3, "<unknown> CPU and prose lines must not fabricate rows");
        assert_eq!(rows[0], KubePod { namespace: "default".into(), pod: "web-7f9c-abc".into(), container: "app".into(), cpu_milli: 12, mem_bytes: 88 << 20 });
        // Same pod, two containers -> two rows (per-container view).
        assert_eq!(&rows[0].pod, &rows[1].pod);
        assert_eq!(rows[2].cpu_milli, 1500);
        assert_eq!(rows[2].mem_bytes, (1.5f64 * (1u64 << 30) as f64) as u64);
    }

    #[test]
    fn top_pods_parses_modern_container_named_name() {
        // Verbatim shape from `kubectl top pods -A --containers` on v1.36 k3s (2026-10-11):
        // the CONTAINER column is labeled NAME and POD exists — NAME must NOT become pod.
        let text = "NAMESPACE                 POD                                      NAME            CPU(cores)   MEMORY(bytes)\n\
arc-system                arc-domination-684cc596-listener         listener        1m           22Mi\n";
        let rows = parse_top_pods(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pod, "arc-domination-684cc596-listener");
        assert_eq!(rows[0].container, "listener");
        assert_eq!(rows[0].cpu_milli, 1);
    }

    #[test]
    fn top_pods_accepts_legacy_name_column() {
        // kubectl <= 1.28 labels the second header token NAME, not POD.
        let text = "NAMESPACE   NAME             CONTAINER   CPU(cores)   MEMORY(bytes)\nk v   w  5m  10Mi\n";
        let rows = parse_top_pods(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pod, "v");
        assert_eq!(rows[0].container, "w");
    }

    #[test]
    fn pvcs_parse_projection_and_sentinels() {
        // Column order follows PVC_COLUMNS exactly: NS NAME PHASE CAP CLASS VOL.
        let text = "db      postgres-data-0   Bound    10Gi   local-path   pvc-abc\n\
             default  temp-scratch      Pending  <none>  -   fast-storage\n\
             truncated line here\n";
        let rows = parse_pvcs(text);
        assert_eq!(rows.len(), 2, "short/malformed lines are dropped");
        assert_eq!(rows[0].capacity_bytes, Some(10u64 << 30));
        assert_eq!(rows[0].class, "local-path");
        assert_eq!(rows[0].volume, "pvc-abc");
        // Unbound claim: capacity "<none>" parses as None (NOT zero), class "-" cleaned.
        assert_eq!(rows[1].phase, "Pending");
        assert_eq!(rows[1].capacity_bytes, None);
        assert_eq!(rows[1].class, "-");
        assert_eq!(rows[1].volume, "fast-storage");
    }
}
