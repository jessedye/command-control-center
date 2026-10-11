# Example configurations

Drop-in starting points for `~/.config/ccc/ccc.conf`. Directive syntax, SSH setup,
and the Kubernetes prerequisites live in [SETUP.md](../SETUP.md); these files cover
the fleet shapes people ask about most.

| File | Fleet shape |
| --- | --- |
| [minimal.conf](minimal.conf) | local box + one SSH station — the smallest useful setup |
| [homelab.conf](homelab.conf) | key-auth SSH fleet + a Kubernetes context, display tuned |
| [gpu-boxes.conf](gpu-boxes.conf) | NVIDIA boxes including unified-memory DGX Spark units (NVRAM panels) |
| [proxmox.conf](proxmox.conf) | a Proxmox VE cluster watched through one seed node (`pve` + `pass` + `discover`) |

All addresses use the RFC 5737 documentation ranges (`192.0.2.x`, `198.51.100.x`,
`203.0.113.x`) — replace them with your own hosts. Labels become tab names: they must be
unique across the fleet and fit 24 cells.

Every example is **safe to experiment with**: parsing is all-or-nothing and happens before
the UI starts, so a typo is printed with its filename, line number, and a hint — it can
never silently shrink your fleet.

```sh
cp docs/examples/minimal.conf ~/.config/ccc/ccc.conf   # edit hosts, then:
ccc
```
