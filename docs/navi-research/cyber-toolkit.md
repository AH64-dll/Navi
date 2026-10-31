# NAVI Research: Cybersecurity Toolkit Inside the Harness (No Kali VM)

Research date: 2026-10-10. Focus: practical pentest capability as agent tools inside
Prime Agent, sandboxed, without a Kali Linux virtual machine.

## Correction to the task brief

The brief says NAVI targets "EndeavourOS/KDE Wayland". **This machine is actually
Fedora 44 Workstation** (`ID=fedora`, kernel 6.19, systemd 259, GNOME-ish PATH).
The research below is written distro-agnostically: every tool listed works on Arch,
Fedora, or in a container. Where Arch packaging matters (user may migrate), it is
noted; where only Fedora matters on the live machine, dnf/toolbox notes apply.
Verified live on this machine: `podman 5.8.1` rootless (`crun`), `skopeo`, `bwrap`,
`systemd-nspawn`, `flatpak` all present; `docker`, `firejail`, `kata` absent.
`mitmproxy`, `httpx` (Python lib), `ncat`, `socat` already installed in
`~/.local/bin`.

---

## 1. Tool availability matrix (no Kali needed)

"Arch repo" = official extra/core. "AUR" = user repo (exact-name `-bin` packages
exist for all ProjectDiscovery tools). "pipx" = PyPI-installable Python tool.
"Static bin" = upstream releases a self-contained Go/Rust binary you drop in
`~/.local/bin`. Verified against archlinux.org packages API, AUR RPC v5, and PyPI
JSON on 2026-10-10.

| Tool | Category | Arch | AUR | pipx | Static binary | Official image |
|---|---|---|---|---|---|---|
| nmap | net recon | extra 7.991 | — | — | — | — |
| masscan | net recon | extra 1.3.2 | — | — | — | — |
| nikto | web scan | extra 2.6.0 | — | — | — | — |
| sqlmap | web exploit | extra 1.10.7 | — | ✅ v1.10.10 | — | — |
| mitmproxy | HTTP proxy | extra 12.2.3 | — | ✅ v12.2.3 | ✅ | — |
| gobuster | dir brute | extra 3.8.2 | — | — | ✅ | — |
| wpscan | WordPress | extra 4.0.1 | — | (ruby gem) | — | wpscanteam/wpscan |
| hydra | brute | extra 9.7 | — | — | — | — |
| john | cracking | extra jumbo | — | — | — | — |
| metasploit | exploit | extra 6.5.5 | — | — | — | metasploitframework/metasploit-framework |
| testssl.sh | TLS | extra 3.2.4 | — | — | — | — |
| trivy | vuln scan | extra 0.75.0 | — | — | ✅ | aquasec/trivy |
| ffuf | fuzzing | — | ✅ ffuf / ffuf-bin | — | ✅ (Go) | — |
| feroxbuster | dir brute | — | ✅ feroxbuster(-bin) | — | ✅ (Rust) | — |
| nuclei | vuln scan | — | ✅ nuclei(-bin) | — | ✅ (Go) | projectdiscovery/nuclei |
| subfinder | subdomains | — | ✅ subfinder(-bin) | — | ✅ (Go) | projectdiscovery/subfinder (20 MB) |
| naabu | port scan | — | ✅ naabu(-bin) | — | ✅ (Go) | projectdiscovery/naabu |
| httpx (toolkit) | HTTP probe | — | ✅ httpx(-bin) | — | ✅ (Go) | projectdiscovery/httpx |
| katana | crawler | — | ✅ katana-bin | — | ✅ (Go) | projectdiscovery/katana |
| dnsx | DNS toolkit | — | ✅ dnsx(-bin) | — | ✅ (Go) | projectdiscovery/dnsx |
| amass | OSINT recon | — | ✅ amass | — | ✅ (Go) | caffix/amass |
| enum4linux | SMB enum | — | ✅ | — | — | — |
| whatweb | fingerprint | — | ✅ | — | — | — |
| wafw00f | WAF detect | — | ✅ | ✅ | — | — |
| gau | URL crawl | — | ✅ | — | ✅ (Go) | — |
| waybackurls | URL crawl | — | ✅ | — | ✅ (Go) | — |
| arjun | param find | — | — | ✅ v2.2.7 | — | — |
| dalfox | XSS | — | ✅ | — | ✅ (Go) | — |
| gowitness | screenshots | — | ✅ | — | ✅ (Go) | — |

**Takeaway:** nothing requires Kali. The ProjectDiscovery stack (subfinder, naabu,
httpx-toolkit, dnsx, katana, nuclei) ships as single Go binaries — `go install`
or AUR `-bin` or the official ~20–365 MB docker images. The classic C stack
(nmap, masscan, hydra, john, nikto, sqlmap) is in every mainstream distro repo.

### Minimal set covering recon + web test + vuln scan

Recommended 10-tool core (covers ~90% of a junior pentest workflow):

```
subfinder → dnsx → naabu → nmap        # recon: subdomains → resolve → port scan
httpx-toolkit → katana → gau           # web recon: probe → crawl → URL history
nuclei (+ nikto as second opinion)     # vuln scan
ffuf or feroxbuster                    # fuzzing / forced browsing
sqlmap                               # injection confirmation
mitmproxy                            # manual/API interception
trivy                                # dependency/image CVE scan (defensive)
```

Optional add-ons by engagement type: wpscan, dalfox, hydra, john, amass,
enum4linux, gowitness, testssl.sh.

---

## 2. MCP servers vs hand-wrapping — what exists

### mcp-kali-server (Wh0am123/MCP-Kali-Server, 841★, MIT)

- Repo inspected directly (README + server.py fetched 2026-10-10).
- Architecture: a **Flask JSON API** (`server.py`, ~20 KB, single file) running on
  the machine that holds the tools + a `client.py` that bridges MCP stdio → that
  HTTP API. Endpoints: `/api/command` (arbitrary exec) plus
  `/api/tools/{nmap,gobuster,dirb,nikto,sqlmap,metasploit,hydra,john,wpscan,enum4linux}`.
- **Does NOT require a Kali host.** `server.py` is ~100 lines of Flask +
  `subprocess.Popen` around binaries on `$PATH`. Kali packaging is just a
  convenience (`sudo apt install mcp-kali-server` exists on Kali). On any Linux
  you `pip install flask` and run `server.py` — it works wherever the tools are.
- **Risk:** it's a thin wrapper with `/api/command` = unauthenticated remote
  shell on whatever port it binds. Default binds 127.0.0.1:5000; keep it there or
  inside a container. Also unmaintained-looking (README last commit 2026-03, code
  is a single 20 KB file, no tests). Fine as a pattern, not as a trust boundary.

### pentest-mcp (DMontgomery40/pentest-mcp, 147★, npm `pentest-mcp`)

- Most polished standalone option found. Node.js MCP server, stdio + streamable
  HTTP + SSE, bearer/OIDC auth on network transport.
- Wraps: nmap, john, hashcat, gobuster, nikto, subfinder, httpx(probe), ffuf,
  nuclei, traffic capture, hydra, priv-esc audit, extraction sweep, wordlist gen,
  report tools (engagement records, client reports, SoW elicitation via MCP
  elicitations).
- Explicitly handles the **Python-httpx vs ProjectDiscovery-httpx name collision**
  (prefers `httpx-toolkit`) — a real issue since the user already has Python
  `httpx` on PATH.
- No Kali needed — it shells out to whatever is on PATH. Install:
  `npm install -g pentest-mcp`, then one `mcpServers` entry.
- Caveat: 147★, single maintainer, "NOT for educational purposes" disclaimer;
  audit before trusting. Good engagement-record/reporting model worth stealing.

### cybersec-toolkit (26zl/cybersec-toolkit, 69★, MIT)

- One-command installer for 670+ tools on Arch/Fedora/Debian/Termux + an
  **authorization-gated** MCP server that runs tools in a disposable **Kata
  Containers** VM per session (local host mode optional). External targets and
  script exec are off by default. 872 agent skills for methodology.
- Closest existing thing to "what NAVI wants": governed exec path + sandbox +
  off-by-default. Heavyweight though — Kata requires a working nested-virt/KVM
  setup; on a 24 GB laptop it's viable but adds a full VM stack the user just
  rejected (Kata ≈ micro-VM, not a container). Worth studying for its
  authorization-gate design more than deploying wholesale.

### redamon (samugit83/redamon, ~3k★) — flagship but heavy

Autonomous pentest framework with attack-surface graph, human approval gates,
Kali Docker sandbox. MCP both directions. Overkill for NAVI's "toolkit" scope
(it's a whole second agent platform) but its human-approval-gate design is a good
reference.

### Others surveyed

zebbern-kali-mcp (48★, dockerized 121 tools), TriV3/MCP-Kali-Server (39★),
k3nn3dy-ai/kali-mcp (25★, Docker+SSE), openbashok/pentest-mcp, several
pentest-osint-mcp-servers — all small, all variations of "wrap binaries behind
MCP". The registry landscape (mcp.so/glama/awesome-mcp-servers) has dozens of
nmap-wrapper MCPs; quality is uniformly low-to-mid. None require Kali; all assume
tools on PATH.

### Wrapping CLI tools yourself as an MCP server

Trivial in Python with the official `mcp` SDK (FastMCP): ~15 lines per tool —
`@mcp.tool()` decorator around a `subprocess.run(["nmap","-sV",target])`, with
arg validation in the signature. Since Prime Agent's MCP client speaks stdio +
streamable HTTP (`settings.json` has `mcpServers`, runtime
`prime-agent-runtime/src/rlm/mcp_base.py` handles both), a `navi-sec-mcp` server
of ~200 lines gives full control over validation, allowlists, timeouts, output
truncation — none of which the off-the-shelf wrappers do well. **This is the
recommended path** (see §5).

### MCP vs Prime-Agent skill

Prime Agent exposes MCP tools to the model as callable tools; a "skill" here is
either a markdown SKILL.md (documentation the model reads + shell commands it
runs via `bash()`) or a Python-backed callable. For pentest tools:

- **MCP** = right choice when you want schema-validated calls, per-tool
  enable/disable policy (`enabledTools`/`disabledTools` in Prime Agent), and
  keep the tool surface enumerable.
- **bash() + SKILL.md** = the model already runs arbitrary shell; a thin
  skill doc teaching flag conventions is zero-infra but zero-guardrail.
- **Recommended hybrid:** a scoped MCP server for *execution* (enforces
  allowlist) + a SKILL.md for *methodology* (recon → probe → scan → verify).

---

## 3. Sandboxing without a VM — options ranked

Hardware: 24 GB RAM, target wants minimal overhead. Options on this machine:

| Option | Isolation | RAM overhead | Verified here | Notes |
|---|---|---|---|---|
| **Podman rootless container** | ns/cgroup/uid-map | ~10–50 MB per container | ✅ podman 5.8.1 rootless works, crun | **Recommended.** No daemon, no root, `podman run --rm` per invocation. |
| **systemd-nspawn** | ns (weaker net isolation by default) | ~10 MB | ✅ present | Container-light; fine but podman has better image ergonomics + `--network` control |
| **bubblewrap (bwrap)** | ns sandbox of a single process | ~1 MB | ✅ present | Lowest overhead; you assemble the sandbox yourself (bind mounts, netns). Good for wrapping single binaries, more DIY. |
| **Firejail** | seccomp+ns profiles | ~1 MB | ❌ not installed | Easy per-app profiles, but setuid helper; weaker than podman for network egress policy |
| **Gondolin micro-VM** | QEMU/libkrun microVM | ~200 MB image + VM RAM | n/a (not installed) | Kernel-level isolation + programmable HTTP egress allowlist + secret injection — genuinely nice, but it IS a (micro-)VM. User said no Kali VM, not no VM at all; still, QEMU dep + 200 MB assets makes it a phase-2 option for truly untrusted code, not the default. |
| **flatpak-spawn** | portal sandbox | low | ✅ flatpak present | Wrong tool — designed for flatpak apps, not ad-hoc tool isolation |
| **Kata Containers** | VM-per-container | ~100–300 MB per pod | ❌ | cybersec-toolkit uses it; heaviest option, needs KVM |

**Verdict: Podman rootless + one purpose-built "navi-sec" image.** Rationale:

- Rootless podman already works on this machine (verified `podman info` →
  `rootless: true`, crun runtime) — zero new infrastructure.
- Container gives: uid-mapped unprivileged user, private netns you can policy,
  `--read-only` rootfs, `--cap-drop=ALL` + selective `NET_RAW` only where needed
  (nmap SYN scan needs `NET_RAW`; httpx/subfinder don't), `--memory`/`--pids-limit`
  caps, `--rm` ephemeral, image pins tool versions.
- RAM: an idle podman container costs megabytes, and NAVI runs tools on demand —
  nothing resident.
- Network isolation: `podman network create navi-sec` (isolated bridge), or
  `--network=none` for offline analysis, or pasta/slirp4netns default for egress.
  Fine-grained egress allowlisting needs either a forward proxy in another
  container (see §4) or Gondolin later.

**The Kali-container nuance:** `kalilinux/kali-rolling` is only **53 MB
compressed** (verified via skopeo on this machine) and runs fine under rootless
podman — it's a container, not a VM. If the user's objection to "Kali VM" was the
RAM/boot overhead, a Kali *container* is arguably the fastest way to get every
tool preinstalled. If the objection was philosophical ("no Kali"), build an
Alpine/Fedora-minimal `navi-sec` image with just the 10-tool core (~300–400 MB,
still smaller than any VM). Recommend the custom image: fewer binaries = smaller
attack surface and clearer scope. Keep `kalilinux/kali-rolling` as an optional
"full toolbox" profile.

---

## 4. Scope-control / authorization guardrails (the part that actually matters)

The tools must not touch unauthorized targets. Defense in depth, cheapest first:

1. **Target allowlist at the MCP layer (mandatory).** Every tool wrapper takes a
   `target` argument; the server validates it against a scope file
   (`~/.config/navi/scope.yaml`: domains, CIDRs, explicit IPs + deny rules like
   RFC1918/external defaults). Reject before exec. pentest-mcp's SoW elicitation
   flow (asks the user for scope via MCP elicitations, attaches it to engagement
   records) is a good model to copy.
2. **Egress policy at the network layer.** The container's only route goes
   through a filtering forward proxy (e.g. `mitmproxy --mode regular` or
   `tinyproxy` + ACL list, or squid) in a sibling container on the same internal
   podman network; `navi-sec` gets no direct egress (`--network=navi-sec-internal`,
   proxy env vars injected). DNS similarly via a scoped resolver. This makes the
   allowlist enforceable even if a tool is tricked into scanning off-scope (tools
   get argv from the model — you can't fully trust the model's argv).
   Simpler interim: `pasta --outbound-if4` / host firewall (nftables) rules per
   container, or just validate hard at layer 1 and log everything.
3. **Rate limiting.** Concurrency cap in the MCP server (semaphore, e.g. max 2
   active scans), per-tool default flags that cap aggression (nmap `-T2..T3`,
   ffuf `-rate`, nuclei `-rl`), wall-clock timeouts per invocation.
4. **Evidence capture.** Per-engagement directory
   `~/.local/share/navi/evidence/<engagement-id>/<timestamp>-<tool>/` mounted
   into the container at `/out`; every call writes raw stdout + a JSON record
   (tool, argv hash, target, started, duration, exit code, output hash). pentest-mcp
   already models this as "engagement records". Required both for reporting and
   for proving scope adherence.
5. **Human approval gate for intrusive tiers.** Tier the tools: passive recon
   (subfinder, gau, waybackurls) → auto-allowed within scope; active scan
   (nmap, httpx, nuclei, nikto) → one-shot user confirm per engagement; intrusive
   (sqlmap exploit flags, hydra, metasploit, john on captured hashes) → explicit
   per-invocation confirm. Prime Agent's approval flow or a `/navi-approve` gate.
6. **Capability hygiene.** `--cap-drop=ALL`, add back `NET_RAW` only for
   nmap/naabu/masscan containers; `--read-only`; `--tmpfs /tmp`; no host mounts
   except the evidence dir (write-only subdir) and the scope file (read-only).
7. **Dangerous defaults to kill:** never run metasploit/hydra containers with
   `NET_ADMIN`/`--privileged`; never bind a tool API to 0.0.0.0; strip
   `/api/command`-style generic exec endpoints from any borrowed server.

---

## 5. Recommended design (ranked by effort)

### Phase A — "scoped MCP + rootless container" (days, recommended)

1. Build `localhost/navi-sec` Containerfile: `fedora-minimal` or `alpine` base +
   the 10-tool core (Go binaries copied from upstream releases, dnf/apk for
   nmap/nikto/sqlmap/mitmproxy). ~350 MB.
2. Write `navi-sec-mcp`: Python FastMCP server, ~200–400 lines. One tool per
   binary: `recon_subdomains`, `recon_resolve`, `scan_ports`, `web_probe`,
   `web_crawl`, `vuln_scan`, `dir_fuzz`, `sql_inject_check`, `mitm_intercept`
   (spawn/attach), `cve_scan`. Each: validate target vs `scope.yaml` → build
   argv → `podman run --rm --network navi-sec-net --cap-drop ALL [-a NET_RAW]
   -v evidence:/out localhost/navi-sec <argv>` → stream/return output + write
   evidence record.
3. Register in Prime Agent `settings.json → mcpServers` (stdio). Tools appear to
   the supervisor model with schemas; `enabledTools` policy can disable tiers.
4. `scope.yaml` + SKILL.md methodology doc + evidence dir conventions.

### Phase B — "off-the-shelf MCP, same sandbox" (hours, quick win)

Install `pentest-mcp` (`npm i -g pentest-mcp`), run it with stdio, and point its
tool resolution at a wrapper dir (`~/bin/navi-sec/` containing shell shims that
exec the podman container). You get its engagement records/reporting free; you
still control exec via the shims + scope check inside the shim. Less clean than
Phase A but near-zero code. Good prototype to validate the workflow before
writing navi-sec-mcp.

### Phase C — "Kali container, no VM" (hours, if user accepts containers)

`podman pull docker.io/kalilinux/kali-rolling` (53 MB) + `apt install
kali-tools-top10` inside a derived image, reusing the Phase A MCP wrapper
unchanged. Maximum tool coverage, still no VM. Risk: 9 MB→GBs of image bloat and
the philosophical Kali question; keep as opt-in profile.

### Phase D — "borrowed governance" (weeks, power-user)

Adopt pieces of `26zl/cybersec-toolkit`: its authorization-gated exec path and
skill corpus, but swap Kata for the Phase A podman sandbox. Or adopt `redamon`
as a separate "autonomous pentest" sub-agent NAVI calls for long engagements.

### Explicitly not recommended

- **Kali VM / virt-manager**: rejected by user, 2+ GB RAM, and unnecessary — every
  tool above runs natively or in a container.
- **systemd-nspawn**: workable but you rebuild podman's image/pull/run ergonomics
  yourself.
- **Firejail**: fine for single apps but its setuid model + weak egress control
  make it worse than podman for this use case.
- **Gondolin as the default**: excellent egress-policy design (JS-programmable
  HTTP allowlist + secret injection) but adds QEMU + microVM weight; revisit for
  the *untrusted generated-code* sandbox (desktop exec tier), not the pentest tier.
- **Raw mcp-kali-server on host**: an unauthenticated exec API glued to Flask —
  borrow the pattern, not the deployment.

---

## Key risks / unverified claims

- **Prime Agent `mcpServers` config shape**: `settings.json` contains an
  `mcpServers` key (currently null) and the runtime handles stdio + streamable
  HTTP in `mcp_base.py`, but the exact per-server JSON schema wasn't verified in
  this pass — confirm against prime-agent docs/source before wiring.
- **pentest-mcp quality**: 147★ single-maintainer npm package; the README is
  thorough (SoW elicitation, engagement records) but the code wasn't audited.
  Treat as prototype, not production trust boundary.
- **cybersec-toolkit Kata dependency**: Kata on Fedora needs KVM; on some laptops
  nested virt is unavailable — would degrade to its `--local` (host) mode.
- **Rootless nmap**: SYN scans need `NET_RAW`; rootless podman *does* support
  granting it to the container's user namespace (the tool runs as fake-root in
  userns — host sees unprivileged). ping/SYN scan behavior under slirp4netns has
  edge cases (ICMP echo works via unprivileged ping_group_range; raw SYN fine).
  Half-verified: flag for a live test on the target machine.
- **mitmproxy-as-egress-policy**: doable but adds config surface; the simpler
  podman-network + host nftables route may be enough for v1.
- **AUR versions verified by name lookup only** (e.g. `katana-bin` 1.8.0, `nuclei`
  3.11.1) — install success not tested on this Fedora host (irrelevant to
  container path anyway).
- The live machine is Fedora 44; if NAVI's true target is EndeavourOS the Arch
  column applies wholesale — nothing in the design is distro-locked since tools
  run in the container.
