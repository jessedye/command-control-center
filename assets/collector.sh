#!/bin/sh
# ccc CCC/1 collector — emitted over SSH by the `ccc` monitor.
#
# Zero-install telemetry for any Linux box with a POSIX shell: sh, dash, busybox ash.
# Emits one CCC/1 frame per invocation using a ~1-second sampling window for
# CPU/process rate deltas. All display strings are percent-hex escaped (%) so
# they survive the `|` field separator (see src/proto.rs).
#
# This script is embedded in the `ccc` binary and shipped to the remote stdin; it
# writes ONLY the protocol to stdout. Any stderr noise (motd, locale warnings) is
# ignored by the decoder.
#
# Portability rules honored (review fix #9): POSIX-only ps options with a /proc
# fallback; integer-safe %.0f for byte counts (BusyBox awk %d wraps at 2^31);
# `sort -nr` (never -g); comm names parsed after the LAST ')' so a program named
# "a(kill)b" cannot shift the stat fields.

LC_ALL=C
export LC_ALL

HZ=$(getconf CLK_TCK 2>/dev/null)
case "$HZ" in '' | 0) HZ=100 ;; esac

TS=$(date +%s 2>/dev/null || echo 0)
NAME=$(uname -n 2>/dev/null || hostname 2>/dev/null || echo unknown)
OSL=""
if [ -r /etc/os-release ]; then
    OSL=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release | head -n 1 | tr -d '"\n\r')
fi
[ -n "$OSL" ] || OSL=$(uname -sr 2>/dev/null || echo unknown)
KERN=$(uname -r 2>/dev/null | tr -d '\n\r' || echo unknown)
LOAD=$(awk '{print $1" "$2" "$3}' /proc/loadavg 2>/dev/null || echo "0 0 0")

# --- snapshot: t0 -----------------------------------------------------
S1=$(grep '^cpu' /proc/stat 2>/dev/null)
N2T0=$(awk '{print $1; exit}' /proc/uptime 2>/dev/null || echo 0)
P1=$(for d in /proc/[0-9]*; do cat "$d/stat" 2>/dev/null; done)

sleep 1

# --- snapshot: t1 -----------------------------------------------------
# Round-2 finding 16: the fraction clock is sampled IMMEDIATELY BEFORE the t1 scan,
# pairing with N2T0 taken immediately before the t0 scan. Reading uptime AFTER the
# scan inflated DT by the scan's own duration, so per-process % was understated.
N2T1=$(awk '{print $1; exit}' /proc/uptime 2>/dev/null || echo 0)
S2=$(grep '^cpu' /proc/stat 2>/dev/null)
N2=$(cat /proc/net/dev 2>/dev/null)
P2=$(for d in /proc/[0-9]*; do cat "$d/stat" 2>/dev/null; done)
UP1=$(awk '{print $1; exit}' /proc/uptime 2>/dev/null || echo 0)
UP_S=$(awk -v u="$UP1" 'BEGIN { printf "%d", u }')
# Fractional window (seconds) between the two scan starts — the basis for honest
# per-process percentages instead of assuming "exactly 1.000 s" (review fix #11).
DT=$(awk -v a="$N2T0" -v b="$N2T1" 'BEGIN { d = b - a; print (d > 0.05) ? d : 1 }')

# Process table source: POSIX ps when it cooperates, /proc scan as fallback
# (minimal busybox builds need not support -o; review fix #9).
PS=$(ps -eo pid=,user=,rss=,args= 2>/dev/null)
if [ -z "$PS" ]; then
    PS=$(for d in /proc/[0-9]*; do
        [ -r "$d/cmdline" ] || continue
        args=$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null)
        case "$args" in '' | ' ') continue ;; esac   # kernel thread: argv-less
        uid=$(sed -n 's/^Uid:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$d/status" 2>/dev/null | head -n 1)
        rss=$(awk '/^VmRSS:/{print $2}' "$d/status" 2>/dev/null)
        pidn=${d##*/}
        printf '%s %s %s %s\n' "$pidn" "${uid:-0}" "${rss:-0}" "$args"
    done)
fi

escstr() { printf '%s' "$1" | tr -d '\n\r' | sed -e 's/%/%25/g' -e 's/|/%7C/g'; }

# --- CPU aggregate + per-core ------------------------------------------
# /proc/stat cpu fields: 2=user 3=nice 4=system 5=idle 6=iowait 7=irq 8=softirq
# 9=steal 10=guest 11=guest_nice. Review fix #5: idle time is $5+$6 (the old
# $4+$5 summed SYSTEM as idle, so a fully sys-busy host reported 0%). Guest time
# is excluded from the total because kernels fold guest time into user — counting
# both inflates the denominator.
CPU_INFO=$(printf '%s\n@@\n%s\n' "$S1" "$S2" | awk '
$1 == "@@" { sw = 1; next }
$1 ~ /^cpu/ {
    tot = 0
    for (i = 2; i <= 9; i++) tot += $i + 0
    idle = $5 + $6
    if (!sw) { pt[$1] = tot; pi[$1] = idle }
    else {
        dt = tot - pt[$1]; di = idle - pi[$1]
        pc = (dt > 0) ? (dt - di) * 100 / dt : 0
        if (pc < 0) pc = 0; if (pc > 100) pc = 100
        if ($1 == "cpu") { tpc = pc }
        else { cs = cs sprintf(" %.1f", pc); n++ }
    }
}
END {
    printf "cpu=%.1f\n", tpc + 0
    sub(/^ /, "", cs)
    printf "cpus=%s\n", cs
    printf "cores=%d\n", n + 0
}')

# --- memory ------------------------------------------------------------
# Round-2 finding 18: MemAvailable: 0 is REAL data (memory exhaustion), not a missing
# field — the old `if (!a)` fallback silently replaced it with free+buffers+cached and
# understated pressure exactly when accuracy matters. Track presence explicitly, and use
# %.0f (finding 17): KiB counts exceed int32 on hosts with >= 2 TiB RAM, where BusyBox
# awk's %d wraps negative.
MEM=$(awk '
/MemTotal:/ { t = $2 }
/MemAvailable:/ { a = $2; have_a = 1 }
/MemFree:/ { f = $2 }
/Buffers:/ { b = $2 }
/^Cached:/ { c = $2 }
/SwapTotal:/ { st = $2 }
/SwapFree:/ { sf = $2 }
END {
    if (!have_a) a = f + b + c
    printf "%.0f %.0f %.0f %.0f", t + 0, a + 0, st + 0, (st + 0 ? sf : 0)
}' /proc/meminfo 2>/dev/null)
[ -n "$MEM" ] || MEM="0 0 0 0"

# --- network: cumulative counters (receiver diffs between ticks) -------
# Review fix #4: split on WHITESPACE (special FS), not /[ \t]+/ — a whitespace-prefixed
# substring made field indices shift (RX read an empty token, TX read "compressed pkts").
# Field layout after "iface:" = 16 counters; RX bytes = f[1], TX bytes = f[9].
NET=$(printf '%s\n' "$N2" | awk '
NR > 2 {
    c = index($0, ":")
    if (c < 2) next
    name = substr($0, 1, c - 1)
    gsub(/^[ \t]+|[ \t]+$/, "", name)
    rest = substr($0, c + 1)
    nf = split(rest, f)
    if (nf >= 16) printf "net=%s|%s|%s\n", esc(name), f[1], f[9]
}
function esc(s) { gsub(/%/, "%25", s); gsub(/\|/, "%7C", s); gsub(/\r/, "", s); return s }')

# --- disks: df -Pk (1024-block POSIX mode) -----------------------------
# %.0f everywhere: GiB-scale byte counts overflow BusyBox awk's %d (int32).
# Review fix #15: `df -Pk` carries NO fstype column ($2 is the size) — the old code emitted
# the block count where the filesystem type belongs. Join /proc/mounts by device name;
# unresolvable types report "-".
# Round-2 finding 12: `/dev/*` alone admits snap's loop-mounted squashfs images — 100%-full
# read-only volumes that fire permanent capacity alerts. Filter by REAL filesystem type
# (mirroring the Rust local collector's list) plus mount-path prefixes and a 1 GiB floor.
# Finding 14: free_bytes must be total-used, not df's AVAILABLE — DiskRow::used_bytes()
# derives used from total-free, so emitting avail there reported every volume as ~0% used
# on filesystems with root-reserve, and pct() overstated it via the double subtraction.
DISKS=$(df -Pk 2>/dev/null | awk '
BEGIN {
    # /proc/mounts: device mountpoint FSTYPE opts dump pass — fstype is field 3.
    while ((getline ln < "/proc/mounts") > 0) {
        n = split(ln, mp, " ")
        if (n >= 3) FT[mp[1]] = mp[3]
    }
    split("ext2 ext3 ext4 btrfs xfs jfs reiserfs f2fs zfs vfat exfat ntfs ntfs3", lf, " ")
    for (i in lf) OK[lf[i]] = 1
    split("/snap/ /var/lib/docker/ /var/lib/containers/ /var/lib/kubelet/ /run/k3s/", px, " ")
}
NR > 1 && $1 ~ /^\/dev\// {
    t = ($1 in FT) ? FT[$1] : "-"
    if (!(t in OK)) next
    m = $6
    for (i = 7; i <= NF; i++) m = m " " $i
    skip = 0
    for (i in px) if (index(m, px[i]) == 1) skip = 1
    if (skip) next
    if ($2 + 0 < 1048576) next   # < 1 GiB: bind-mount noise, not a capacity
    gsub(/\r/, "", m)
    gsub(/%/, "%25", m); gsub(/\|/, "%7C", m)
    fr = ($2 - $3) * 1024; if (fr < 0) fr = 0
    printf "disk=%s|%s|%.0f|%.0f|%.0f\n", m, t, $2 * 1024, fr, $4 * 1024
}' | sort -u)

# --- top processes: measured-window CPU delta + ps join -----------------
MT=$(printf '%s' "$MEM" | awk '{print $1 + 0}')
[ "$MT" -gt 0 ] 2>/dev/null || MT=1
PROC_ALL=$(printf '%s\n@@\n%s\n@@\n%s\n' "$P1" "$P2" "$PS" | awk -v HZ="$HZ" -v DT="$DT" -v MT="$MT" '
function stat_parse(line, pidkey,   sp, cp, i, rest, arr, n) {
    sp = index(line, " ")
    if (sp == 0) return 0
    cp = 0
    # LAST ")" — a comm containing ")" shifts positional fields under first-")" parsing.
    for (i = length(line); i > sp; i--)
        if (substr(line, i, 1) == ")") { cp = i; break }
    if (cp == 0) return 0
    rest = substr(line, cp + 2)
    n = split(rest, arr, " ")
    if (n < 20) return 0
    PV[pidkey] = arr[12] + arr[13]          # utime + stime (clock ticks)
    ST[pidkey] = arr[20] + 0                 # starttime, identity across snapshots
    return 1
}
$1 == "@@" { sec++; next }
sec == 0 { pid = substr($0, 1, index($0, " ") - 1); if (pid) stat_parse($0, pid); next }
sec == 1 {
    pid = substr($0, 1, index($0, " ") - 1)
    if (!(pid in PV)) next
    base = PV[pid]; baseStart = ST[pid]
    if (stat_parse($0, pid)) {
        if (ST[pid] != baseStart) next      # pid reuse between snapshots
        d[pid] = PV[pid] - base
        if (d[pid] < 0) d[pid] = 0
        st_keep[pid] = ST[pid]
    }
}
sec == 2 {
    # Fields: pid user rss args...  (args may contain spaces; never positional past $3)
    pid = $1
    if (!(pid in d)) next
    cmd = ""
    for (i = 4; i <= NF; i++) cmd = cmd (cmd == "" ? "" : " ") $i
    # Kernel threads surface as "[name]" argv — local collection drops them (empty cmd);
    # do the same here so both lanes show workloads, not cpuhp/kworker rows.
    if (substr(cmd, 1, 1) == "[") next
    cpu = d[pid] * 100.0 / HZ / DT           # percent of ONE core over the MEASURED window
    # Elapsed since process start: uptime minus starttime-in-seconds (review fix #11:
    # the old code subtracted an EPOCH timestamp from uptime, so every row read "0:00:00").
    el = int(uptime - st_keep[pid] / HZ); if (el < 0) el = 0
    name = $4
    slash = 0
    for (i = 1; i <= length(name); i++) if (substr(name, i, 1) == "/") slash = i
    if (slash > 0) name = substr(name, slash + 1)
    if (name == "") name = "?"
    # Round-2 finding 10: emit THREE sortable keys (cpu, mem%, pid) TAB-prefixed so the
    # shell can keep the UNION of each ranking. Sorting only by CPU dropped idle memory
    # hogs and low-PID processes before Rust ever saw them, making MEM/PID sorts useless.
    # Finding 17: %.0f for RSS KiB (int32 overflow at ~2 TiB RSS).
    printf "%10.2f\t%10.2f\t%d\tproc=%d|%s|%s|%.1f|%.1f|%.0f|%.0f|%s\n", \
        cpu, 100.0 * $3 / MT, pid, pid, esc(name), esc($2), cpu, 100.0 * $3 / MT, $3, el, \
        esc(substr(cmd, 1, 90))
}
function esc(s) {
    gsub(/%/, "%25", s); gsub(/\|/, "%7C", s); gsub(/\r/, "", s)
    return s
}
BEGIN { getline uptime < "/proc/uptime"; split(uptime, ua, " "); uptime = ua[1] + 0 }
'
)
TAB=$(printf '\t')
# Three rankings (CPU desc, MEM desc, PID asc), each capped at 48, deduped keeping first
# occurrence — the union matches LocalCollector's PROC_KEEP selection on this side of the wire.
proc_top() {
    printf '%s\n' "$PROC_ALL" | grep -v '^$' \
        | sort -t "$TAB" -k"$1" | cut -d "$TAB" -f4- | head -n 48
}
PROCS=$(
    { proc_top '1,1nr'; proc_top '2,2nr'; proc_top '3,3n'; } | awk '!seen[$0]++'
)

# --- GPU: NVIDIA first, AMD sysfs fallback ------------------------------
GPUS=""
if command -v nvidia-smi >/dev/null 2>&1; then
    NV_ARGS="--query-gpu=name,memory.used,memory.total,utilization.gpu,temperature.gpu --format=csv,noheader,nounits"
    # Round-2 finding 24: a wedged driver hangs `nvidia-smi` in D-state and used to silence
    # the WHOLE frame (the query runs before emission). Probe under `timeout` where the
    # distro has one (GNU coreutils and busybox both ship it; absence degrades to old behavior).
    if command -v timeout >/dev/null 2>&1; then
        NVOUT=$(timeout 5 nvidia-smi $NV_ARGS 2>/dev/null)
    else
        NVOUT=$(nvidia-smi $NV_ARGS 2>/dev/null)
    fi
    # Round-2 finding 4: `[N/A]` / `[Not Supported]` are LEGAL nvidia-smi responses. The old
    # emitter shipped them into mandatory numeric fields; strict CCC/1 then rejected the
    # ENTIRE host frame for an unreadable temperature sensor. Validate numerics per row:
    # unusable memory/util drops just that GPU row, a non-numeric temperature becomes -1
    # (decode's None sentinel). Rows shorter than five columns cannot index safely — skip.
    # Integrated/unified GPUs (DGX Spark GB10 and friends) answer [N/A] for FB memory:
    # nvidia-smi has NO query exposing usage of the unified LPDDR5x pool. Best available
    # per-GPU signal: sum of CUDA processes' allocations (MiB) against host MemTotal —
    # truthful NVRAM usage for owners watching inference boxes (owner request 2026-10-11).
    # Single-instance assumption (one integrated GPU per box): the allocation total is
    # applied to every row whose own FB memory came back N/A.
    CA_SUM=-1
    MT_MIB=0
    if command -v timeout >/dev/null 2>&1; then
        CA_RAW=$(timeout 5 nvidia-smi --query-compute-apps=used_memory --format=csv,noheader,nounits 2>/dev/null)
    else
        CA_RAW=$(nvidia-smi --query-compute-apps=used_memory --format=csv,noheader,nounits 2>/dev/null)
    fi
    if [ -n "$CA_RAW" ]; then
        CA_SUM=$(printf '%s\n' "$CA_RAW" | awk '/^[ ]*[0-9]+([.][0-9]+)?[ ]*$/ {gsub(/ /,""); s+=$1} END {printf "%d", s}')
    fi
    MT_MIB=$(awk '/^MemTotal:/ {printf "%d", $2/1024}' /proc/meminfo 2>/dev/null)
    [ -n "$MT_MIB" ] || MT_MIB=0
    GPUS=$(printf '%s\n' "$NVOUT" | awk -F', ' -v alloc="$CA_SUM" -v pool="$MT_MIB" '
    NF < 5 { next }
    {
        # Names may legally contain commas ("Tesla T4, Rev A") — rebuild everything left
        # of the four trailing numeric columns so short rows keep their full name.
        name = ""
        for (i = 1; i <= NF - 4; i++) name = name (name == "" ? "" : ", ") $i
        gsub(/^ +| +$/, "", name)
        mu = $(NF-3); mt = $(NF-2); uu = $(NF-1); tt = $NF
        gsub(/^ +| +$|\[| [A-Za-z].*$/, "", mu)
        gsub(/^ +| +$|\[| [A-Za-z].*$/, "", mt)
        gsub(/^ +| +$|\[| [A-Za-z].*$/, "", uu)
        gsub(/^ +| +$/, "", tt)
        if (name == "") name = "NVIDIA GPU"
        num = "^[0-9]+([.][0-9]+)?$"
        # Round-2 finding 4 kept: a non-numeric UTILIZATION drops the row (it is the gauge).
        if (uu !~ num) next
        # Unified-memory rows: N/A memory becomes -1 (wire sentinel = unknown), filled from
        # the shell-side probes where possible. Rows with both fields unknowable still emit
        # (util/temp are what owners watch on GB10 boxes; decode maps -1 -> None).
        if (mu !~ num) mu = -1
        if (mt !~ num) mt = -1
        if (mu + 0 < 0 && alloc + 0 >= 0) mu = alloc
        if (mt + 0 < 0 && pool + 0 > 0) mt = pool
        if (tt ~ /^-?[0-9]+([.][0-9]+)?$/) { temp = tt + 0 } else { temp = -1 }
        gsub(/%/, "%25", name); gsub(/\|/, "%7C", name); gsub(/\r/, "", name)
        printf "gpu=nvidia|%s|%.0f|%.0f|%.1f|%.0f\n", name, mu + 0, mt + 0, uu + 0, temp
    }')
fi
if [ -z "$GPUS" ]; then
    GPUS=$(for cd in /sys/class/drm/card[0-9]*/device; do
        [ -r "$cd/mem_info_vram_total" ] || continue
        vt=$(cat "$cd/mem_info_vram_total" 2>/dev/null)
        vu=$(cat "$cd/mem_info_vram_used" 2>/dev/null || echo 0)
        bu=$(cat "$cd/gpu_busy_percent" 2>/dev/null || echo 0)
        ms=$(cat "$cd"/hwmon/hwmon*/temp1_input 2>/dev/null | head -n 1)
        tc=-1
        [ -n "$ms" ] && tc=$((ms / 1000))
        pid_id=$(sed -n 's/^PCI_ID=//p' "$cd/uevent" 2>/dev/null | head -n 1)
        nm="AMD Radeon"
        [ -n "$pid_id" ] && nm="AMD Radeon [$pid_id]"
        printf 'gpu=amdgpu|%s|%d|%d|%s|%s\n' "$nm" "$((vu / 1048576))" "$((vt / 1048576))" "$bu" "$tc"
    done)
fi

# --- AI inference listeners ---------------------------------------------
# Optional and agentless: Python's standard library discovers full argv and scrapes
# loopback /metrics in parallel. Hosts without python3 simply omit these rows.
INFERENCE=""
if command -v python3 >/dev/null 2>&1; then
INFERENCE=$(python3 - 2>/dev/null <<'PY'
import concurrent.futures
import glob
import math
import subprocess
import urllib.request

MAX = 4 * 1024 * 1024
GENERATED = {
    "vllm:generation_tokens_total", "vllm:generation_tokens",
    "sglang:generation_tokens_total", "sglang:generated_tokens_total",
    "tgi_request_generated_tokens_total", "tgi_request_generated_tokens_sum",
    "trt_llm:generation_tokens_total", "nv_trt_llm_generation_tokens_total",
    "ollama_generated_tokens_total", "ollama:generation_tokens_total",
    "ollama:eval_count_total", "llamacpp:tokens_predicted_total",
    "llamacpp:tokens_predicted", "lmdeploy:generation_tokens_total",
}
PROMPT = {
    "vllm:prompt_tokens_total", "vllm:prompt_tokens",
    "sglang:prompt_tokens_total", "tgi_request_input_tokens_total",
    "tgi_request_input_length_sum", "trt_llm:prompt_tokens_total",
    "nv_trt_llm_prompt_tokens_total", "ollama_prompt_tokens_total",
    "ollama:prompt_tokens_total", "ollama:prompt_eval_count_total",
    "llamacpp:prompt_tokens_total", "lmdeploy:prompt_tokens_total",
}
ACTIVE = {
    "vllm:num_requests_running", "sglang:num_running_reqs",
    "tgi_batch_current_size", "ollama_active_requests",
    "ollama:num_requests_running", "llamacpp:requests_processing",
    "lmdeploy:num_requests_running",
}
QUEUED = {
    "vllm:num_requests_waiting", "sglang:num_queue_reqs", "tgi_queue_size",
    "ollama_pending_requests", "llamacpp:requests_deferred",
    "lmdeploy:num_requests_waiting",
}

def executable(args, name):
    return any(a.rsplit("/", 1)[-1].lower() in (name, name + ".exe") for a in args)

def option(args, names):
    for i, arg in enumerate(args):
        for name in names:
            if arg == name and i + 1 < len(args):
                return args[i + 1]
            if arg.startswith(name + "="):
                return arg.split("=", 1)[1]
    return None

def identify(args):
    lower = [a.lower() for a in args]
    has = lambda s: any(s in a for a in lower)
    if (executable(lower, "vllm") and "serve" in lower) or has("vllm.entrypoints.openai.api_server"):
        return "vLLM", 8000
    if has("sglang.launch_server") or has("sgl.launch_server"):
        return "SGLang", 30000
    if executable(lower, "text-generation-launcher"):
        return "TGI", 80
    if executable(lower, "trtllm-serve") or has("tensorrt_llm.serve"):
        return "TensorRT-LLM", 8000
    if executable(lower, "tritonserver"):
        return "TensorRT-LLM", 8002
    if executable(lower, "ollama") and "serve" in lower:
        return "Ollama", 11434
    if executable(lower, "llama-server"):
        return "llama.cpp", 8080
    if executable(lower, "lmdeploy") and any(lower[i:i+2] == ["serve", "api_server"] for i in range(len(lower)-1)):
        return "LMDeploy", 23333
    return None

def discover():
    found = {}
    for path in glob.glob("/proc/[0-9]*/cmdline"):
        try:
            args = [a.decode("utf-8", "replace") for a in open(path, "rb").read().split(b"\0") if a]
        except (OSError, IOError):
            continue
        kind = identify(args) if args else None
        if not kind:
            continue
        harness, default_port = kind
        try:
            port = int(option(args, ("--metrics-port", "--port", "--http-port")) or default_port)
            if not 1 <= port <= 65535:
                continue
        except ValueError:
            continue
        host = option(args, ("--host", "--http-address", "--address")) or "127.0.0.1"
        host = host.strip("[]")
        if host in ("", "0.0.0.0", "::", "*"):
            host = "127.0.0.1"
        elif host == "localhost":
            host = "127.0.0.1"
        if "/" in host or any(c.isspace() for c in host):
            continue
        endpoint = ("[%s]:%d" if ":" in host else "%s:%d") % (host, port)
        key = harness + "|" + endpoint
        if key not in found:
            found[key] = (harness, endpoint, host, int(path.split("/")[2]), port)
    return sorted(found.values())

def resolve_container_ports(servers):
    containerized = False
    for server in servers:
        try:
            cgroup = open("/proc/%d/cgroup" % server[3]).read()
            if any(k in cgroup for k in ("docker", "kubepods", "containerd", "libpod")):
                containerized = True
                break
        except (OSError, IOError):
            pass
    if not containerized:
        return servers
    template = "{{.State.Pid}} {{range $port, $bindings := .NetworkSettings.Ports}}{{if $bindings}}{{$port}}={{(index $bindings 0).HostIp}}:{{(index $bindings 0).HostPort}},{{end}}{{end}}"
    for runtime in ("docker", "podman"):
        try:
            ps = subprocess.run([runtime, "ps", "-q"], capture_output=True, text=True, timeout=2)
            if ps.returncode:
                continue
            ids = ps.stdout.split()
            if not ids:
                return servers
            inspected = subprocess.run([runtime, "inspect", "--format", template] + ids, capture_output=True, text=True, timeout=2)
            if inspected.returncode:
                continue
        except (OSError, subprocess.SubprocessError):
            continue
        by_pid = {}
        for line in inspected.stdout.splitlines():
            fields = line.split()
            if not fields or not fields[0].isdigit():
                continue
            by_pid[int(fields[0])] = fields[1:]
        resolved = []
        for harness, endpoint, host, pid, port in servers:
            for mapping in by_pid.get(pid, []):
                prefix = "%d/tcp=" % port
                if not mapping.startswith(prefix):
                    continue
                hostport = mapping[len(prefix):].rstrip(",")
                if ":" not in hostport:
                    continue
                mapped_host, mapped_port = hostport.rsplit(":", 1)
                if mapped_host in ("", "0.0.0.0", "::"):
                    mapped_host = "127.0.0.1"
                if mapped_port.isdigit():
                    host, port = mapped_host, int(mapped_port)
                    endpoint = ("[%s]:%d" if ":" in host else "%s:%d") % (host, port)
            resolved.append((harness, endpoint, host, pid, port))
        return resolved
    return servers

def add(metrics, key, value):
    metrics[key] = metrics.get(key, 0.0) + max(0.0, value)

def scrape(server):
    harness, endpoint, host, pid, port = server
    url_host = "[%s]" % host if ":" in host else host
    request = urllib.request.Request("http://%s:%d/metrics" % (url_host, port), headers={"Accept": "text/plain"})
    metrics = {}
    try:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=0.75) as response:
            body = response.read(MAX + 1)
            if len(body) > MAX:
                raise ValueError("metrics too large")
        for line in body.decode("utf-8", "replace").splitlines():
            if not line or line.startswith("#"):
                continue
            ends = [i for i in (line.find("{"), line.find(" "), line.find("\t")) if i >= 0]
            if not ends:
                continue
            name = line[:min(ends)]
            try:
                value = float(line.split()[-1])
            except (ValueError, IndexError):
                continue
            if not math.isfinite(value):
                continue
            if name in GENERATED:
                add(metrics, "generated", value)
            if name in PROMPT:
                add(metrics, "prompt", value)
            if name in ACTIVE:
                add(metrics, "active", value)
            if name in QUEUED:
                add(metrics, "queued", value)
            if name == "sglang:gen_throughput":
                metrics["native"] = max(0.0, value)
            if name == "nv_trt_llm_runtime_metrics":
                if 'metric_type="num_requests_running"' in line:
                    add(metrics, "active", value)
                elif 'metric_type="num_requests_waiting"' in line:
                    add(metrics, "queued", value)
    except Exception:
        pass
    return harness, endpoint, pid, metrics

def esc(value):
    return str(value).replace("%", "%25").replace("|", "%7C").replace("\r", "").replace("\n", "")

servers = resolve_container_ports(discover())
with concurrent.futures.ThreadPoolExecutor(max_workers=min(8, len(servers) or 1)) as pool:
    for harness, endpoint, pid, metrics in pool.map(scrape, servers):
        fields = [metrics.get(k, -1) for k in ("active", "queued", "prompt", "generated", "native")]
        print("inference=%s|%s|%d|%s" % (esc(harness), esc(endpoint), pid, "|".join("%.6f" % v for v in fields)))
PY
)
fi

# --- frame --------------------------------------------------------------
printf 'CCC/1\n'
printf 'name=%s\n' "$(escstr "$NAME")"
printf 'ts=%s\n' "$TS"
printf 'os=%s\n' "$(escstr "$OSL")"
printf 'kernel=%s\n' "$(escstr "$KERN")"
printf 'up=%s\n' "$UP_S"
printf 'load=%s\n' "$LOAD"
printf '%s\n' "$CPU_INFO"
printf 'mem=%s\n' "$MEM"
printf '%s\n' "$DISKS"
printf '%s\n' "$NET"
printf '%s\n' "$GPUS"
printf '%s\n' "$INFERENCE"
printf '%s\n' "$PROCS"
printf 'end\n'
