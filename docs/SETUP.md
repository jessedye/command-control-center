# Setup guide

How to get `ccc` watching your machines: the box you run it on, remote stations over
plain SSH, and a Kubernetes fleet via `kubectl`. Nothing is ever installed on the
monitored machines.

## 1. Build

```sh
git clone https://github.com/jessedye/command-control-center
cd command-control-center
cargo build --release        # Rust 1.95+ toolchain
./target/release/ccc         # starts with the LOCAL station
```

Requirements on the machine running `ccc`:

| Need | Why |
| --- | --- |
| Linux (primary target) | reads `/proc`, `/sys`; macOS/Windows show partial panels |
| `ssh` binary | remote stations reuse your OpenSSH config/keys/agent |
| `kubectl` (optional) | only for the `--k8s` fleet tab |
| `nvidia-smi` (optional) | GPU/NVRAM panel; absent = panel hides |

`ccc` needs root for nothing. It reads the same files any user can; process users come
from `/etc/passwd`, not from privilege.

## 2. The local station

Just run `ccc`. The first frame primes ~1 second of CPU/network sampling so every gauge
starts with real numbers. Keys: `TAB`/`1..9` switch stations, `s` sorts processes,
`+`/`-` grows/shrinks the table, `v` verbose, `SPACE` pauses, `q` quits.

Scriptable snapshot (cron, pastes, CI):

```sh
ccc --once --width 100 --height 40
ccc --once --no-color > state.txt     # plain text, single frame, exit 0
```

## 3. SSH stations (zero install)

### 3.1 One-time: key-based access (passwords work too)

`ccc` runs `ssh -T -o BatchMode=yes <spec> sh -s`, so password prompts never appear —
key auth must already work. Verify with:

```sh
ssh -T myserver true && echo "ready for ccc"
```

If that asks for a password, fix auth first (`ssh-copy-id myserver`). If `myserver` is an
alias in `~/.ssh/config` (with port, user, jump host baked in), use the alias directly —
`ccc` honors all of it.

The first connection per host pays the handshake; later polls ride an OpenSSH control
socket (`ControlPersist=300` in `$XDG_RUNTIME_DIR`), so a poll costs one round-trip, not
a login. Poll interval defaults to 3 s (`--interval 1` faster, `--interval 10` gentler).

Password auth is fully supported as a fallback (`pass FILE`, mode 600 — `sshpass -f` reads
it; the password never touches argv, config parsing, or logs), and ssh tries the key first,
so both can be configured at once. **Prefer keys for polled stations.** Every password
handshake counts against server-side login throttling (OpenSSH ≥9.8 rate caps, PAM
faillock): exceed the burst allowance and you get `kex_exchange_identification: Connection
reset by peer` from manual logins, then lockouts that extend on every reattempt. One-way
fix: deploy keys (`ssh-copy-id`) and let the control socket carry everything.

If `ssh-copy-id` succeeds yet BatchMode logins are refused, check the CLIENT policy before
blaming the server: RHEL/Fedora FIPS crypto policies drop `ssh-ed25519` from OpenSSH's
default `PubkeyAcceptedAlgorithms`, so the client silently skips your key before offering
it (`debug1: Skipping ... id_ed25519 - corresponding algorithm not in
PubkeyAcceptedAlgorithms`). Re-add the algorithm per host in `~/.ssh/config`:

```
Host myserver
    IdentityFile ~/.ssh/id_ed25519
    PubkeyAcceptedAlgorithms +ssh-ed25519
```

### 3.2 Adding stations

```sh
# one remote, labeled automatically from the destination
ccc -H myserver

# label anything with `label=` prefix; everything after it is passed to ssh verbatim
ccc -H 'rack4=-p 2222 admin@10.0.0.4' \
    -H 'edge=-i ~/.ssh/edge_key root@edge.example.com' \
    -H 'bastion-J=jumphost -W %h:%p web01.internal'

# quoted values need quoting twice if they contain spaces (shell_split is quote-aware)
ccc -H 'lab=-i "~/.ssh/ed25519 key" labhost'
```

Rules:

- The destination host must be the LAST token of the spec (after any flags it consumes).
- Option values bind correctly for `-b -B -c -D -E -e -F -I -i -J -l -L -m -O -o -p -Q
  -R -S -W -w`; unusual grouped forms (`-46C`) are passed through uninterpreted.
- A station that fails shows `⚠ STALE <reason>` over its last good data; the link failure
  also raises a red `LINK DOWN` alert line. It never blanks to zero silently.
- Each remote poll is hard-bounded at 25 s; a wedged host cannot freeze the dashboard.

### 3.3 What runs on the remote

`ccc` pipes an embedded POSIX shell script (`assets/collector.sh`) into `sh -s` on each
poll: read `/proc/stat`, `/proc/meminfo`, `/proc/net/dev`, `df -Pk`, `ps`/`/proc`, probe
`nvidia-smi` under `timeout` where available, print one `CCC/1` frame, exit. No daemon,
no package, no writes, no root. It works under bash, dash, and BusyBox ash — check any
candidate node with:

```sh
sh assets/collector.sh | head    # prints a frame with CCC/1 … end
```

## 4. Config file — named tabs at every launch

Flags are for one-off runs; day-to-day you want `ccc` to open your machines by name. Define
them once in a config file — tabs along the top read **fedora**, **rack4**, **K8S:homelab**,
and TAB/←/→ or `1`..`9` jumps between them.

The default location is `$XDG_CONFIG_HOME/ccc/ccc.conf` (typically `~/.config/ccc/ccc.conf`).
Keep several profiles and launch by name:

```sh
ccc                          # loads ~/.config/ccc/ccc.conf
ccc --config ~/ccc/prod.conf # a second profile: prod stations, prod kube context
ccc --no-config              # ignore the default file (local station only)
```

One directive per line, whitespace-separated and quote-aware; `#` comments allowed full-line
and inline:

```ini
# stations — shown as tabs, in this order
local  fedora                       # this machine, labeled
ssh    rack4   -p 2222 admin@10.0.0.4
ssh    keybox  -i ~/.ssh/edge_key root@edge.example.com
k8s    default   homelab            # kube context "default", displayed K8S:homelab

# settings (CLI flags override these)
interval 3
top      20
warn     70
crit     90
sort     mem
verbose
focus    fedora                     # tab opened on start: label, prefix (K8S:), 1-based number, or "overview"
summary  overview                    # quick-stats strip: own Overview tab (default), every tab (all), off
throttle_wait 5                      # seconds before each station's ONE login-throttle retry
```

Rules that keep surprises out:

- **CLI wins wholesale.** If any `-H`/`--k8s` appears on the command line, the file's station
  lines are ignored entirely — no half-merged fleets. Scalar settings merge value-by-value:
  CLI > config file > built-in defaults.
- Launching with `--k8s` opens focused **on the fleet tab** — you asked for the cluster, you
  land on it. `--focus`/`focus` overrides that choice.
- Plain `ccc` lands on the **Overview** tab (tab 1): every station as one colored row of
  cpu / mem / nvram / load / disk. Station tabs carry only their own detail panels; the
  quick-stats strip appears nowhere else. `summary all` restores the strip on every tab;
  `summary off` removes it and the Overview tab, so tab numbers shift back by one.
  With Overview present, `--focus 2` is your first station, not the local tab.
- One `k8s` line per profile (run a second `ccc --config ...` window for a second cluster).
- Labels become tab names; they must be unique across the fleet.
- **A bad config fails fast and loud.** Unknown directives, duplicate labels, unparsable
  values, missing required arguments — every problem is printed with the filename and line
  number before the UI starts, so a typo can never silently shrink your fleet:

```text
$ ccc
ccc: /home/you/.config/ccc/ccc.conf: config line 2: unknown directive 'hsot' (expected: local, ssh, k8s, interval, top, warn, crit, sort, verbose, no-color, focus)
```

## 5. Kubernetes fleet tab

### 5.1 Prerequisites

`ccc --k8s` shells out to the `kubectl` on your PATH using your normal
`~/.kube/config` context. Point it elsewhere:

```sh
export KUBECONFIG=~/clusters/prod.yaml   # honored like any kubectl user
ccc --k8s                    # current-context
ccc --k8s prod-cluster       # named context (tab labeled K8S:prod-cluster)
```

Quick sanity checks before blaming `ccc`:

```sh
kubectl config current-context
kubectl get nodes --no-headers | wc -l
kubectl top nodes            # this powers the CPU%/MEM% columns
```

### 5.2 metrics-server (the CPU/MEM columns)

Node status, roles, versions, and readiness come from `kubectl get nodes` alone. The
per-node CPU%/MEM% numbers come from `kubectl top`, i.e. **metrics-server** — the API-aggregated
add-on. If it's missing, `ccc` still lists the fleet, colors `NotReady`/`Unknown` nodes
red, and prints exactly why metrics are blank.

Worked example — a real 10-node k3s homelab (`default` context), zero changes needed.
Put it in `~/.config/ccc/ccc.conf` (§4):

```ini
local fedora
k8s   default   homelab
```

```sh
kubectl --context default top nodes        # all 10 nodes return CPU/MEM → nothing to install
ccc                                        # tabs: fedora · K8S:homelab — pick the fleet, numbers fill in
ccc --k8s default                          # ad-hoc equivalent, auto-focused on the fleet tab
```

`metrics-server` is built into k3s and usually pre-installed; RKE2/k3s clusters typically
need no setup at all, and `kubectl top nodes` returning rows is the entire prerequisite.
Note that plain `--k8s` follows your kubeconfig **current-context**, so on a machine where
that points at an unrelated cluster (e.g. a cloud EKS cluster), name the context explicitly.

Install on stock clusters (needs DNS working in-cluster):

```sh
# vanilla kubeadm / generic manifests
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml

# minikube: built-in add-on
minikube addons enable metrics-server

# self-signed kubelet certs (very common homelab case): the stock manifest will hang
# in CrashLoop with "x509: certificate signed by unknown authority". Patch it:
kubectl -n kube-system patch deploy metrics-server --type=json -p='[
  {"op":"add","path":"/spec/template/spec/containers/0/args",
   "value":["--kubelet-insecure-tls"]}]'
```

Verify: `kubectl top nodes` returns rows within ~1–2 minutes of pod start.

RBAC note: `kubectl top` needs `get` on `nodes/metrics` in the API group
`metrics.k8s.io`. Read-only service accounts that can `get nodes` commonly lack it; grant:

```sh
kubectl create clusterrolebinding ccc-viewer --clusterrole=view --user=<your-identity>
```

(`view` covers `nodes` + `nodes/metrics` reads. Scope tighter with your own ClusterRole
if your policy demands.)

### 5.3 Reading the fleet panel

- Chip row `NODES 7/8` — ready count vs total (cordoned nodes stay Ready: `Ready,SchedulingDisabled`
  is counted healthy, deliberately).
- Non-Ready nodes render red with their raw STATUS string.
- CPU%/MEM% columns are `kubectl top` usage vs allocatable; `-` means no metric (new node,
  metrics-server down, missing RBAC — the footnote quotes kubectl's actual error).
- **CPU/MEM columns** are smooth bars + exact percent from `kubectl top` — usage vs
  allocatable for that node. A `-` means no metric (new node, metrics-server down, missing
  RBAC — the footnote quotes kubectl's actual error).
- **TOP PODS** ranks containers by live CPU (`kubectl top pods -A --containers`). The bar is
  *relative to the hottest container in the poll* — a heat map answering "what is hot", not
  an absolute percentage. Kubernetes exposes containers, not host PIDs: for true per-node OS
  process tables add SSH stations pointing at the nodes themselves.
- **PVC** lists persistent volume claims with phase and provisioned capacity
  (`Bound` green, `Pending` amber, anything else red). Metrics-server does not measure
  filesystem usage *inside* a volume — that needs Prometheus/kubelet stats — so ccc reports
  capacity truthfully instead of inventing a percentage.
- Pod metrics need `get`+`list` on `pods` and `pods/metrics` (metrics.k8s.io group) beyond
  the node grants above.
- The fleet tab coexists with SSH tabs: `ccc --k8s -H 'node1=k8s-node1.example'` gives you
  both depths — fleet rollup plus full per-machine gauges on the same boxes.

## 6. Tuning knobs

| Flag | Effect |
| --- | --- |
| `--interval 5` | seconds between samples (0.25…3600) |
| `--top 20` | process rows kept per sort (1…40; the union of CPU/MEM/PID rankings is collected so sorting MEM shows real hogs) |
| `--warn 70 --crit 90` | gauge thresholds; alerts fire at crit (must satisfy crit ≥ warn) |
| `--sort mem` | initial process sort |
| `--verbose` | full command lines, more interfaces/disks/nodes |
| `--station NAME` | label for the local tab |
| `--no-color` | monochrome (also honors `$NO_COLOR`) |
| `--config FILE` | load stations + settings from a specific profile file |
| `--no-config` | ignore the default config file (station flags only) |
| `--focus NAME\|N` | open on a tab by label, label prefix, or 1-based number |

## 7. Troubleshooting

| Symptom | Meaning / fix |
| --- | --- |
| Station shows `ssh exited 255 Permission denied` | key auth not set up for that user/host — see §3.1 |
| `ssh exited 255 ... Name or service not known` | hostname only resolvable via your SSH config — use the alias |
| Station stuck `STALE` with last-known data | link died; keep reading cached telemetry (by design); check network/auth |
| `timed out after 25.0s` | remote too slow to run the collector in-window; raise `--interval` or investigate load |
| NET panel shows 0 B/s everywhere right after start | normal: rates are interval deltas; first frame is the warm-up window |
| Fleet shows `-` for CPU/MEM | metrics-server missing/unhealthy, or RBAC — see §5.2 |
| TOP PODS header present, rows show "pod metrics unavailable" | missing `pods`/`pods/metrics` RBAC for your identity, or metrics-server too old |
| No PVC section | missing `get persistentvolumeclaims` permission, or cluster has no claims |
| Disk list empty over SSH | collector found no fstypes in its allowlist (ZFS datasets mount outside `/dev/*` — file an issue) |
| `Killed`/`Out of memory` on tiny VPS during sample | the ps fallback scans `/proc`; reduce `--top`, raise interval |

## 8. Security posture

- Credentials live in OpenSSH, never in `ccc`: keys, agents, JumpHosts, `ProxyCommand`
  all work because `ccc` executes your installed `ssh`.
- Remote side executes exactly what `ssh … sh -s` feeds stdin: a read-mostly report script.
  It creates no files, makes no outbound connections, and dies when stdout closes.
- Every field decoded from a remote frame is range-checked, finiteness-checked, and
  control-character sanitized before it reaches your terminal — a malicious remote cannot
  inject escape sequences into the dashboard.
- `ccc` sends NO data anywhere except the ssh destinations you name. There is no telemetry.

## 9. Dependencies: checking and installing

`ccc` shells out to three tools, each required only by the feature you configure:

- **ssh** — every remote station (`-H`) and every Proxmox seed (`-P`). The collector rides
  `ssh <host> sh -s`; nothing is installed on the far side. Package: `openssh-clients`
  (Fedora/RHEL), `openssh-client` (Debian/Ubuntu).
- **sshpass** — password-mode auth. When `--pass FILE` (or a config `pass` line) is present,
  ccc wraps ssh as `sshpass -f FILE ssh …`; without the binary every password station fails.
  Package: `sshpass` (EPEL on RHEL-family). Key-based stations never need it.
- **kubectl** — the Kubernetes tab only. It talks to clusters through your kubeconfig, so
  existing contexts, namespaces, and auth plugins keep working. Not shipped by every distro:
  Fedora has `kubernetes-client`; Debian/Ubuntu users typically install from upstream
  (<https://kubernetes.io/docs/tasks/tools/>); `ccc` prints exactly that guidance for you.

### What the check looks like

Before connecting anywhere, startup probes each tool this launch will exec. Missing tools on
an interactive terminal get an offer:

```
ccc: 1 optional tool(s) missing for this launch:
  • sshpass — --pass password auth (feeds the password to ssh non-interactively)
Package manager detected: Dnf
Install 'sshpass' now? [y]es ([dnf] package) · [d] download instructions · [n] skip > y
running: dnf install -y sshpass
sshpass installed.
```

- `y` runs the package manager's install (prefixed with `sudo` unless you are root), then
  re-probes; success continues straight into the dashboard.
- `d` prints the exact package command (and the upstream project site where relevant) and
  leaves the choice of running it to you.
- `n` skips — if the launch still needs the tool, ccc exits with code 3 and says why,
  instead of opening tabs that fail at every sample.
- Non-interactive launches (`--once`, cron, pipes) print the same hint lines and exit 3 when
  anything required is missing; prompts would just hang.

Local-only dashboards (`ccc` with no `-H`/`--k8s`/`-P` and no config stations) require no
external binaries at all, so the check is silent. Ceph health on PVE uses `ceph status
--format json | python3` **on the hypervisor** over the existing SSH session — there is no
client-side dependency for it.
