<div align="center">

# cryptocli

**Multi-coin CPU mining with a live TUI, and a daemon that keeps going after you close it.**

[![ci](https://github.com/codingsushi79/ccli/actions/workflows/ci.yml/badge.svg)](https://github.com/codingsushi79/ccli/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![rust](https://img.shields.io/badge/rust-stable-orange.svg)

</div>

<img width="3001" height="1695" alt="image" src="https://github.com/user-attachments/assets/44e7b928-5543-4e72-a73c-84afc2ecacde" />


## Install

```bash
curl -fsSL https://raw.githubusercontent.com/codingsushi79/ccli/master/install.sh | bash
```

Builds from source, installs as `cryptocli` (and `ccli`), and tells you exactly
what to add to your shell rc if the install directory isn't on your `PATH`.
Needs Rust — the script tells you how to get it if you don't have it.

<details>
<summary>Manual install</summary>

```bash
git clone https://github.com/codingsushi79/ccli
cd ccli
cargo install --path .
```

Environment overrides for the script: `CCLI_INSTALL_DIR`, `CCLI_REPO`,
`CCLI_BRANCH`, `CCLI_NO_COLOR`.
</details>

## Quick start

Run `cryptocli` and press `a` to add a wallet, a rig or an endpoint — no need
to leave the dashboard. Or from the shell:

```bash
cryptocli wallet add main --coin BTC --address bc1qyouraddress...
cryptocli rig add btc --url stratum+tcp://pool.example.com:3333 --wallet main --coin BTC
cryptocli
```

Already checked a pool with `endpoint add`? Point the rig at it by name and the
url *and* credentials come along, so nothing is typed twice:

```bash
cryptocli endpoint add unmineable --url stratum+tcp://sha256.unmineable.com:3333 \
  --user 'BTC:youraddress.worker' --password x
cryptocli endpoint test unmineable      # confirm the pool takes your worker
cryptocli rig add btc --url unmineable --coin BTC
```

The `--url` of a rig accepts either a literal pool address or the name of a
stratum endpoint. The TUI's rig form does the same — type an endpoint name in
the `pool` field.

Press `q` to close the dashboard. **Mining continues.** Run `cryptocli` again to
come back to the same session.

Opening the dashboard never starts mining by itself: if `cryptocli` has to bring
the daemon up, it comes up idle, and quitting shuts it back down unless
something is actually mining. Mining starts when you ask for it — `S` in the
dashboard, or `cryptocli start`.

## What it does

- **Real SHA-256d stratum mining.** Full stratum v1: subscribe, authorize,
  `mining.notify`, difficulty changes, extranonce handling, share submission.
  The header assembly is pinned to the Bitcoin genesis block in the test suite.
- **Fast.** An AVX2 eight-way SHA-256 hashes eight nonces per vector step —
  about **6.6× the scalar path** on a CPU without SHA-NI (7.9 → 52 MH/s on four
  threads of a Xeon W-2255). Falls back to scalar automatically.
- **Multi-coin, two ways.** Run separate rigs, *or* let one rig mine several
  coins at once and split its threads between them. Both work at the same time.
- **Rigs cooperate instead of duplicating work.** Pools often hand the same
  `extranonce1` to every connection on an account, so independent sessions
  would search overlapping space. A process-wide coordinator makes the
  extranonce2 sequence a shared resource keyed by `(pool, extranonce1)` — no two
  threads anywhere ever cover the same ground.
- **Survives the UI.** Mining runs in a detached daemon with no controlling
  terminal. `q`, `Esc`, `Ctrl-C`, or closing the terminal only closes the
  dashboard. Stopping is always explicit.
- **Detailed hardware view.** Per-core utilisation, memory, load, every thermal
  sensor, and NVIDIA GPUs when `nvidia-smi` is present.
- **Pluggable check endpoints.** Register an HTTP status URL *or a stratum pool
  address*. Pool checks connect, subscribe and authorize with your worker
  credentials, so you can confirm a pool will accept your login before building
  a rig around it. HTTP checks pull named values out of the JSON response.
- **One dashboard for many machines.** Each box runs its own daemon; one of
  them connects to the others and merges everything into a single view —
  combined hashrate, per-coin and per-node totals, and start/stop for a rig on
  any machine without sshing to it.
- **Wallet connection.** Payout addresses only. cryptocli never asks for, holds,
  or transmits a private key or seed phrase.

> **Expectations.** CPU SHA-256d against Bitcoin earns effectively nothing —
> ASICs are ~10 orders of magnitude faster, and some pools will drop a worker
> producing shares this slowly. This is a correct and genuinely fast miner,
> useful for pool testing, private and low-difficulty chains, merge-mined
> chains, and learning how stratum actually works. It will not pay your rent.

## Mining more than one coin

Two arrangements, both supported at once.

**Separate rigs** — managed independently, started and stopped on their own:

```bash
cryptocli rig add btc --url stratum+tcp://pool-a:3333 --wallet main --coin BTC
cryptocli rig add ltc --url stratum+tcp://pool-b:3333 --wallet lite --coin LTC
```

**One rig, several coins** — one unit to start and stop, threads divided by
weight:

```bash
cryptocli rig add combo --url stratum+tcp://pool-a:3333 --wallet main --coin BTC --threads 8
cryptocli rig coin combo --coin LTC --url stratum+tcp://pool-b:3333 --wallet lite --weight 1
```

That rig becomes two sessions, `combo/BTC` and `combo/LTC`, sharing its eight
threads. In the TUI, press `c` on a rig to do the same thing. The dashboard
aggregates per coin no matter which arrangement you use.

## Many machines, one dashboard

Every machine mines on its own; one of them shows you all of them. On each
machine you want to watch:

```bash
cryptocli remote enable    # prints the address, a token, and a fingerprint
cryptocli daemon stop && cryptocli daemon start
```

Then, on the machine you want to watch *from* — or press `a` on the Nodes tab
and fill in the same four fields:

```bash
cryptocli node add rig2 --address 192.168.1.50:9944 \
    --token <TOKEN> --fingerprint sha256:<FINGERPRINT>
```

`--address` takes a bare host too; the port defaults to 9944. Host names that
resolve to several addresses are all tried, so a machine with both an IPv6 and
an IPv4 record connects even when the peer is listening on IPv4 only.

The connection is verified before it's saved, so a wrong token, address or
fingerprint fails immediately instead of becoming a permanently red row, and
the failure says what to check rather than only an OS error number. On the
Nodes tab, `t` retries the selected machine right away — a machine that has
been unreachable for a while is otherwise retried on a widening interval, up to
once a minute. The dashboard gains a **Nodes** tab and a `NODE` column, totals
combine across machines, and rigs can be controlled remotely:

```bash
cryptocli stop rig2:btc-main       # node:rig addresses another machine
cryptocli start rig2:btc-main
cryptocli node test rig2           # one-shot connectivity check
```

In the TUI, `s`/`x`/`±`/`d` all act on whichever machine owns the selected rig.
When a node goes unreachable its rigs show `unknown` rather than a stale
`mining` — the hub reports what it knows, not what it last saw.

### How the link is secured

Connections between machines are TLS (rustls), and the certificate is **pinned
by fingerprint** — the SSH model rather than the CA model, because a mining box
has no public hostname a CA could vouch for.

- On first `remote enable`, the machine generates a self-signed certificate.
  The private key stays put, mode `0600`.
- `node add --fingerprint` pins exactly that certificate. Anything else is
  refused with a mismatch error naming both fingerprints.
- Omitting `--fingerprint` trusts what the peer presents right now and prints
  it so you can compare against `cryptocli remote show` on that machine. That
  is trust-on-first-use: convenient, and the one moment an interceptor could
  substitute its own key.
- The token is only sent **after** the encrypted channel is up, so it never
  crosses the network in the clear.

Remote access stays off unless you set both a listen address and a token. Even
with TLS, prefer a LAN or VPN address over a public interface — pinning stops
interception, it does not make the port safe to expose.

## Keys

| Key | Action |
|---|---|
| `1`–`6`, `Tab` | switch view |
| `↑ ↓` / `j k` | move selection (scroll, in Logs) |
| `a` | add a rig / wallet / endpoint / machine, depending on the view |
| `c` | mine another coin on the selected rig, at the same time |
| `d` | remove the selected item from the config |
| `e` | enable or disable the selected rig |
| `s` / `x` | start / stop the selected rig |
| `S` / `X` | start all enabled / stop all |
| `+` / `-` | add or remove a thread on the selected rig, live |
| `p` | run the selected endpoint check now |
| `3` | the Nodes view: every machine's health at a glance |
| `t` | in Nodes: reconnect to the selected machine now |
| `r` | reload the config file |
| `f` | freeze the display (mining is unaffected) |
| `Q` | shut the daemon down |
| `q` / `Esc` / `Ctrl-C` | close the dashboard, keep mining |
| `?` | help |

In a form, `←` `→` `Home` `End` (or `Ctrl-A` / `Ctrl-E`) move within a field and
`Delete` removes the character under the cursor, so a typo in a long pool url or
address is a two-keystroke fix. A form the daemon rejects stays open with
everything you typed still in it.

## Commands

```
cryptocli                          open the dashboard
cryptocli status [--json]          one-shot summary, scriptable
cryptocli start [RIG] / stop [RIG] start or stop a rig, or all of them
cryptocli bench --threads 8        measure local hashrate, no pool needed
cryptocli algos                    list supported algorithms

cryptocli wallet   add|list|rm
cryptocli rig      add|list|rm|enable|disable|threads|coin|uncoin
cryptocli endpoint add|list|rm|test
cryptocli node     add|list|rm|test
cryptocli remote   enable|disable|show
cryptocli daemon   start|stop|status|log|reload|run
cryptocli config   path|show|init
```

Config edits made through the CLI or the TUI are picked up by a running daemon
automatically. Hand edits need `cryptocli daemon reload` (or `r` in the TUI).

## Check endpoints

The "anything worth watching" piece. Register a URL; the daemon polls it on its
own interval and surfaces the result. Two kinds, picked automatically from the
URL.

**Stratum pools** — `stratum+tcp://`, `stratum://`, `tcp://`, or a bare
`host:port`. (TLS pools, `stratum+ssl://`, are not implemented yet and are
rejected with a clear message rather than silently connecting in the clear.) cryptocli connects, subscribes, and authorizes with the
credentials you give it, so the check answers the question you actually have
about a pool: *will it take my worker?*

```bash
cryptocli endpoint add unmineable \
  --url stratum+tcp://sha256.unmineable.com:3333 \
  --user 'BTC:youraddress.worker' \
  --password x
```

```
$ cryptocli endpoint test unmineable
ok  pool handshake ok  84 ms
  authorized: yes
  difficulty: 16384
  extranonce1: c33661e0
  job: 6a2f
```

Bad credentials are reported as such rather than as a dead pool:

```
FAILED  pool check failed  149 ms
  authorized: no
  error: worker rejected: Your address seems to be invalid...
```

Leave `--user` off to check reachability only — no credentials are sent. This
is the quickest way to validate a pool login *before* configuring a rig around
it.

**HTTP** — anything starting with `http://` or `https://`.

```bash
cryptocli endpoint add pool-stats \
  --url https://pool.example.com/api/worker/rig1 \
  --header 'Authorization: Bearer TOKEN' \
  --interval 30 \
  --field hashrate=data.hashrate \
  --field balance=data.miner.balance
```

`--user`/`--password` become HTTP basic auth here, since plenty of stats APIs
want that instead of a header. `--field label=path` extracts values using a
dotted path with optional array indices (`data.workers[0].name`).

The dashboard shows extracted values, status, latency and rolling uptime; `p`
re-checks the selected endpoint immediately, and `a` adds one without leaving
the UI. `cryptocli endpoint test NAME` runs a single check in the foreground
without involving the daemon.

## Configuration

`~/.config/cryptocli/config.toml`; state in `~/.local/share/cryptocli/`. Set
`CRYPTOCLI_HOME` to relocate both.

```toml
[settings]
max_threads = 0          # 0 = every logical core except one
sample_interval_ms = 1000
autostart = true

[[wallet]]
name = "main"
coin = "BTC"
address = "bc1q..."

[[rig]]
name = "combo"
url = "stratum+tcp://pool-a:3333"
coin = "BTC"
wallet = "main"
threads = 8
enabled = true

  # A second coin on the same rig, mined at the same time.
  [[rig.target]]
  coin = "LTC"
  url = "stratum+tcp://pool-b:3333"
  wallet = "lite"
  weight = 1

# Another machine, merged into this dashboard.
[[node]]
name = "rig2"
address = "192.168.1.50:9944"
token = "..."
fingerprint = "sha256:..."   # pinned TLS certificate

# Accept dashboards from other machines (off unless both are set).
[settings.remote]
listen = "0.0.0.0:9944"
token = "..."

[[endpoint]]
name = "pool-stats"
url = "https://pool.example.com/api/worker/rig1"
interval_secs = 60

[endpoint.fields]
balance = "data.balance"

# A stratum pool check: connects, subscribes and authorizes.
[[endpoint]]
name = "unmineable"
url = "stratum+tcp://sha256.unmineable.com:3333"
user = "BTC:youraddress.worker"
password = "x"
interval_secs = 120
```

## Design

```
cryptocli (TUI)  ──unix socket, JSON lines──▶  cryptocli daemon (hub)
   thin client                                  ├── rig "combo/BTC" ── stratum ── N threads ─┐
   renders snapshots                            ├── rig "combo/LTC" ── stratum ── N threads ─┤
   holds no state                               ├── rig "solo"      ── stratum ── N threads ─┤
                                                ├── work coordinator ◀───────────────────────┘
                                                ├── hardware sampler
                                                ├── endpoint poller
                                                └── peers ──TLS + token──▶ daemon on rig2
                                                                         ▶ daemon on rig3
```

Peer polling lives in the daemon rather than the TUI, so the combined view
exists whether or not anyone is watching, and one slow machine can never stall
the dashboard. A peer answering a snapshot request returns *its own* machine
only, so a mesh can't double-count.

Notes on the parts that matter:

- **Vectorised hashing.** Eight nonces per AVX2 step. Each lane costs two
  compressions: the header's first 64 bytes never change within a work unit, so
  their midstate is computed once. `top()` returns only the digest's most
  significant word — all the share filter needs — and the full 32 bytes are
  materialised only for the ~1-in-4-billion hashes that pass.
- **No dynamic dispatch per hash.** `Algorithm` is a trait object resolved once
  per thread, which hands off to a worker monomorphised over a concrete hasher.
- **Cheap work handoff.** New jobs are published by bumping a generation
  counter, so the steady state is one relaxed atomic load per 2048 hashes
  rather than a mutex per hash. Hash counters flush per batch to keep threads
  off each other's cache lines.
- **No duplicated search space.** The nonce range is partitioned across a
  session's threads, and extranonce2 allocation is coordinated process-wide, so
  two rigs on one pool cooperate rather than repeat each other.
- **Async only for I/O.** Tokio drives sockets and timers; it never touches the
  hot path.

## Tests

```bash
cargo test
```

Covers the SHA-256d hasher against the Bitcoin genesis block, the AVX2 path
against the scalar one lane by lane, difficulty-to-target conversion, the fast
share filter against a full comparison at several difficulties (including
fractional), stratum job parsing, work-coordinator uniqueness under concurrency,
form validation, certificate fingerprinting and pin normalisation, and JSON
path extraction.

The miner has also been validated end to end against a mock stratum pool that
independently rebuilds the block header from each submitted share and checks the
hash against the target — every submitted share was accepted.

## License

MIT. See [LICENSE](LICENSE).
