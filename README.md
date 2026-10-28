# ccc — Command & Control Center

A low-overhead terminal dashboard for your fleet of machines: CPU, memory, GPU VRAM,
disks, network, and top processes as readable ASCII gauges — local box, remote hosts over
plain SSH, and Kubernetes node fleets via `kubectl`. One static binary on your side; nothing
to install on theirs.

Tabbing through a live 12-station fleet — Overview board, station detail panels with per-station
top processes, and DGX Spark nodes showing unified-memory NVRAM:

![Tabbing through the ccc dashboard](docs/demo.gif)

## Why

`htop` watches one machine. Dashboards aimed at fleets want agents, daemons, registries,
and a YAML file before you see a single number. `ccc` is the middle ground: a single Rust
binary with std threads (no async runtime), an embedded POSIX collector script that runs
under bash, dash, BusyBox ash, or any `sh`, and strict output framing so partial garbage
from a flaky link never renders as data. The whole binary is ~2 MB and idles in the low
single-digit megabytes of RSS.

## Install

```sh
cargo build --release          # -> target/release/ccc  (Rust 1.95+)
```

## Dependencies

`ccc` itself is a single Rust binary with no runtime libraries. Three external command-line
tools are used by specific features, so each is required only when that feature is configured:

| Tool | Needed for | Package (Fedora / Debian) |
| --- | --- | --- |
| `ssh` | remote stations (`-H`) and Proxmox seeds (`-P`) | openssh-clients / openssh-client |
| `sshpass` | password auth (`--pass FILE` or config `pass`) | sshpass (EPEL on RHEL) / sshpass |
| `kubectl` | the Kubernetes tab (`--k8s`, config `k8s`) | kubernetes-client / snap install kubectl --classic |

At startup ccc checks every tool the launch will actually exec — a local-only run asks for
nothing. For each missing tool on an interactive terminal it offers to install it through your
detected package manager:

```
ccc: 1 optional tool(s) missing for this launch:
  • sshpass — --pass password auth (feeds the password to ssh non-interactively)
Package manager detected: Dnf
Install 'sshpass' now? [y]es ([dnf] package) · [d] download instructions · [n] skip > y
running: dnf install -y sshpass
sshpass installed.
```

`y` runs the install (sudo-prefixed unless you're root) and re-checks; `d` prints the
install command and upstream link instead of running anything; `n` skips. In non-interactive
launches (`--once`, cron, pipes) there is nothing to prompt, so ccc prints the same hints and
exits with code **3** rather than starting a session whose remote tabs would fail at every
sample. Ceph health on Proxmox clusters is collected over SSH using python3 on the hypervisor
itself — your workstation needs nothing extra.

## Run

Set up your fleet once and every launch after that is just `ccc`:

```sh
cat > ~/.config/ccc/ccc.conf <<'CONF'
local fedora                  # your machine
ssh   rack4   -p 2222 admin@10.0.0.4
k8s   default homelab         # kube context, tab reads K8S:homelab
focus fedora
CONF

ccc                           # tabs along the top: fedora · rack4 · K8S:homelab
```

Ready-to-copy annotated configs for common setups live in
[docs/examples/](docs/examples/) — a minimal two-box fleet, a mixed homelab (SSH + Kubernetes),
GPU boxes with unified-memory NVRAM, and a Proxmox cluster seed. All addresses use RFC 5737
documentation ranges; swap in your own hosts.

Ad-hoc runs use flags; any `-H`/`--k8s` replaces the config's stations wholesale so a
one-off invocation can't silently merge into your daily fleet:

```sh
ccc --once                    # render one frame to stdout and exit (scriptable)
ccc --config ~/ccc/prod.conf  # multiple profiles, launched by name
ccc --interval 1              # sampling period in seconds (minimum 0.25)
ccc -H 'lab=-p 2222 10.0.0.7' # add a remote station; label first, then ssh args + host
ccc --k8s staging-ctx         # ad-hoc fleet — opens focused on it, no TAB needed
ccc --focus K8S               # start on the tab matching a name/prefix/number
ccc --warn 70 --crit 90       # gauge thresholds (percent)
ccc --top 20                  # process rows per station
```

Remote stations need only a working `ssh`: `ccc` pipes its embedded collector over stdin
(`ssh <host> sh -s`) — no agent, no package, no writes to the remote filesystem. If plain
`ssh <host> uname` works, `ccc -H` works.

Full walkthrough — SSH fleet setup, metrics-server/RBAC for the Kubernetes tab, tuning
knobs, troubleshooting — is in the [setup guide](docs/SETUP.md).

### Keys

| Key | Action |
| --- | --- |
| `TAB` / `→` / `1`–`9` | switch tabs (tab 1 is the Overview fleet board) |
| `←` | previous tab |
| `s` | sort processes by CPU / MEM / PID |
| `+` / `-` | more / fewer process rows |
| `v` | verbose (full command lines) |
| `SPACE` | pause updates |
| `q`, `Esc` | quit |

## What's on screen

- **Overview** (tab 1) — the consolidated fleet board: one row per station with cpu / mem /
  nvram / load / disk / AI input/output tokens/sec, colored by threshold, plus PVE cluster rows showing hypervisors
  online and guests running. Station tabs then show *only* their own detail panels.
  Configure placement with `summary overview|all|off` in the config file.
- **Right rail** — fleet-wide CPU / MEM / NVRAM / DISK bars, color-banded against
  `--warn` / `--crit`.
- **CPU** — aggregate util, per-core sparkline strip, rolling history.
- **MEM** — used/available, swap, rolling history.
- **NVRAM** — NVIDIA utilization, temperature, and VRAM allocation (auto-probed; degrades
  silently where there is no GPU). Integrated/unified parts that answer `[N/A]` for FB
  memory (DGX Spark's GB10, other SoC GPUs) still render fully: utilization + temperature
  plus unified-memory usage as the sum of CUDA process allocations over host RAM.
- **AI SESSIONS** — every discovered inference-server listener on the station, with active
  and queued requests plus input/output tokens/sec. Auto-detects **vLLM, SGLang, Hugging
  Face TGI, TensorRT-LLM/Triton, Ollama, llama.cpp (`llama-server`), and LMDeploy**, including
  custom `--port`/`--host` arguments and multiple listeners. Containerized servers are followed
  through their published Docker/Podman ports. Rates come from each listener's loopback
  `/metrics` counters over the real sample interval (or its native throughput gauge
  when counters are unavailable). These are listener-wide totals, not separate client-request
  sessions. The first sample and harnesses that do not export a metric show `—`, never a fake
  zero. Remote discovery uses `python3`'s standard library when present; no package, agent,
  credentials, or remote file installation is needed, and other telemetry is unaffected when
  Python or metrics are unavailable.
- **NET** — true interval deltas (bytes/sec) per interface, with link speed where known.
- **DISK** — real mounts with fstype, usage bar, free/total.
- **TOP PROCS** — union of the top processes by CPU *and* by memory, so a big idle
  allocation can't hide behind busy small ones.
- **Alerts** — red banner lines for anything past `--crit`, counted in the footer. A dead
  SSH link keeps the last good frame on screen stamped `⚠ STALE` instead of blanking.

The Kubernetes tab lists every node with inline CPU/MEM heat bars from `kubectl top`,
ranks your workloads by live usage (TOP PODS, per container), and inventories storage
claims with phase and provisioned size (PVC). Non-`Ready` nodes render red, and when
metrics-server is missing the panel says exactly why instead of showing blanks.

## How it works

One sampler thread per station feeds the renderer over a channel; a tiny reader thread
forwards keypresses. Everything is std: threads + `mpsc`, no tokio, no daemon. Every
external command (`ssh`, `nvidia-smi`, `kubectl`) runs under a hard deadline with drained
pipes, so hung tools can wedge neither the UI nor the process exit.

Data crosses the wire as the line-oriented **CCC/1** protocol: `ccc|1` opens a frame,
keyed rows carry positional fields, `end` closes it. The parser rejects duplicate frames,
out-of-order keys, truncated rows, non-finite numbers, and counter wrap — corruption from
a lossy link surfaces as a station error, never as a plausible-looking number.

## Development

```sh
cargo test                     # unit + CLI integration tests
./target/release/ccc --once    # headless single frame (CI-friendly, scriptable)
```

## License

MIT — see [LICENSE](LICENSE).
