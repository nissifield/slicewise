# slicewise: technical design v0.2 (Rust)

**Status:** Design v0.2 (Rust), not built.

slicewise is a small, unprivileged Linux resource allocator for machines that run an orchestrator (for example an AI-agent gateway or a CI runner) together with its delegated workloads: builds, coding-agent runs and batch jobs. It is one Rust binary. Launched as a drop-in for `systemd-run`, it puts each workload into a class slice under the user's systemd manager. It decides at launch whether the workload fits, and it gives the workload a parallelism hint. Run as a small daemon, it reads PSI pressure and memory headroom and adjusts a few cgroup v2 knobs within fixed bounds. When the orchestrator or an interactive workload needs room, batch work is slowed first. When nothing else needs the machine, batch work can use all of it. Everything runs locally: there is a Unix socket, no network listener and no telemetry. Every change to a limit is logged with its reason, and `slicewise explain <unit>` replays them.

---

## 1. Summary

The project name is defined in one place: the `NAME` constant in `src/main.rs`, plus `[package].name` in `Cargo.toml`. Slice names, the socket name, the config directory and the `SLICEWISE_*` environment prefix are all derived from it. Renaming the project means two edits and a docs search-and-replace.

## 2. Problem and intent

Static per-unit caps such as `CPUQuota=` and `MemoryMax=` are set for the worst case and are wrong most of the time:

- **Idle capacity is wasted.** A single build on an otherwise idle 8-core / 32 GB box (illustrative example) is still held to its quota. A CPU quota is not work-conserving, while a CPU weight is.
- **Memory caps overlap and still OOM.** Per-task caps that add up to more than RAM give no protection. A shared cap fixes the total, but two heavy jobs admitted together are then OOM-killed inside it. Without swap, memory that is already allocated cannot be taken back. The only levers are admission (not starting work that does not fit) and choosing the victim.
- **Parallelism is fixed at launch.** A build reads `nproc` or `available_parallelism()` once, at start. Later limit changes cannot change `-j`.
- **No intent.** The kernel cannot tell "a human is waiting on this" from "this can run overnight".

"Fully utilised intelligently" means these measurable properties (the load-scenario test in §10 checks them):

| ID | Property | Measure |
|---|---|---|
| U1 | A lone workload can use the machine | It gets ≥ 90 % of `C = online CPUs − reserve_cpus` and may grow to the pool memory budget |
| U2 | The orchestrator is never starved | Protected-unit `cpu.pressure` some avg10 ≤ 10 % once settled (≤ 30 s) under full batch load |
| U3 | No avoidable OOM | 0 `oom_kill` events when the declared size hints fit the pool: the second job queues instead of both dying |
| U4 | No idle capacity while work waits | Batch work is never queued while memory headroom ≥ its need for 2 consecutive ticks |
| U5 | Explainable | Every limit change has a log line with its reason, and `explain` answers for any managed unit |

## 3. Goals, non-goals, anti-goals

**Goals (v1)**
- One static binary that runs unprivileged on cgroup v2, through the systemd user manager and the controllers delegated to it (`cpu` and `memory` are required).
- Workload classes declared by the orchestrator, admission control for memory, and two reactive rules: a memory brake and a batch CPU clamp.
- A drop-in replacement for `systemd-run`.
- Fail-safe defaults. Works with no daemon running.

**Non-goals (v1)**
- GPU, the io and cpuset controllers, a root/system-wide mode, multiple hosts, scheduling deadlines, per-task memory lending, live `-j` changes, and killing or requeueing work.

**Anti-goals (never)**
- A network listener, telemetry, or a cloud dependency.
- A scheduler or queueing system that replaces the orchestrator. slicewise only says admit, queue or reject, and adjusts limits.
- A sandbox. Workloads are trusted at the user's own privilege level.
- Writing to the orchestrator's own unit at runtime.
- Lowering any `memory.max`.

## 4. Approaches considered

| # | Approach | Pros | Cons |
|---|---|---|---|
| A | **Null option:** tune systemd statically (weights instead of quotas, a shared pool slice, `MemoryLow` on the orchestrator) | No code. Gets about 70 % of the CPU win, because weights are work-conserving | Cannot prevent two jobs OOMing each other inside the pool; no notion of "can wait"; no `-j` hint; no protection of the orchestrator's latency beyond weights; nothing to explain |
| B | **Purely reactive daemon:** watch PSI and usage, shift every knob per unit | Needs nothing from the orchestrator | Cannot tell urgent work from work that can wait; without swap, memory cannot be moved after it is allocated; many knobs × many units invites oscillation; the most code |
| C | **Intent only:** the orchestrator sends priorities and sizes, and limits are fixed at launch | Simple and predictable | The orchestrator cannot see machine state; limits never adapt; every orchestrator has to re-implement the logic |
| D | **Hybrid:** intent sets class and size at launch; the daemon admits, then reacts within class bounds | Uses the orchestrator's knowledge (urgency, expected size) and the kernel's knowledge (pressure, headroom); few knobs | Needs a small daemon plus a wrapper |
| E | Existing tools (systemd-oomd, earlyoom/nohang, desktop resource daemons, Kubernetes/Nomad/Slurm) | Mature | They kill rather than prevent, need root or a system daemon, or are far too heavy for one box |

**Decision (Claude): D, the hybrid. A is shipped as its first layer.**

1. The static frame from A (a pool slice with a hard memory cap, class slices with weights, `MemoryLow` and a higher weight for the orchestrator) is generated by `slicewise units`. It is useful on its own, and it is what everything falls back to.
2. Intent is limited to what changes a decision: a **class** (interactive, normal or batch, where batch means "can wait"), an optional **memory size hint** and an optional **jobs hint**.
3. The daemon adds only what A cannot do: **admission** (batch work queues when it would not fit), a **memory brake** (when the pool nears full, batch growth is throttled first), a **batch CPU clamp** (when protected or interactive work shows CPU pressure) and a **launch-time jobs hint**.

**v1:** everything above, plus `status`, `explain`, `reset` and `check-config`. **M2:** `adopt` patterns for units the orchestrator starts outside the wrapper.

**Backlog (each with a trigger):**
- Deadlines and numeric priorities, when an orchestrator actually sends them.
- Changing a running unit's class (`boost`), when users ask to promote a running job.
- A daemon-owned make/cargo **jobserver** for live `-j` changes, when wrong `-j` is measured to cost more than 15 % of wall time.
- Clamping the normal class, when protected PSI stays high while batch is already at its floor.
- Killing and requeueing preemptible batch work, when the orchestrator supports retries.
- io and cpuset controllers, when they are delegated or the hardware is NUMA or big.LITTLE.
- GPU, when there is a GPU host and a sharing mechanism a user can control (device cgroup, MPS or MIG).
- A root mode, when someone needs to manage system services.
- `ManagedOOMMemoryPressure=` documentation for hosts that run systemd-oomd.

## 5. Architecture

### 5.1 Components

There are three, all in the one binary:

```
orchestrator / human
   │  slicewise run --class batch --mem-hint 3G -- <systemd-run args>
   ▼
[wrapper] ──(1 JSON line over $XDG_RUNTIME_DIR/slicewise.sock)──► [daemon]
   │  admit/queue reply, jobs = N                                    │ every tick (2 s): read cgroupfs + /proc
   │  exec systemd-run --user --slice=app-slicewise-batch.slice …    │ policy (pure fn) → actions
   ▼                                                                 │ systemctl --user set-property --runtime …
transient unit in class slice ◄──────────────────────────────────────┘
static frame (unit files from `slicewise units`): pool slice, 3 class slices, slicewise.service
```

- **Static frame.** Plain systemd unit files written once by `slicewise units <dir>`. They hold every hard limit. The daemon never changes them.
- **Wrapper (`slicewise run`).** Asks the daemon for a decision, then `exec`s `systemd-run`. If the daemon is unreachable, it decides locally with the same policy function and launches anyway (fail-open for launch).
- **Daemon (`slicewise daemon`).** One control-loop thread, plus one socket thread that handles each connection in a short-lived thread. It runs as `slicewise.service` with a tiny footprint (`MemoryMax=64M`). It has no persistent state: everything is re-derived from cgroupfs and systemd at startup.

### 5.2 Workload model

**Decision (Claude): four classes, one slice per class.** The class is read from a unit's parent slice, so the daemon needs no state to know it. Aggregate weight per class does not depend on how many tasks a class has, and a whole class can be braked or clamped with one write.

| Class | Where | CPU weight (static) | Reactive treatment | Admission | OOM order (`OOMScoreAdjust`) |
|---|---|---|---|---|---|
| protected | the orchestrator's own unit(s), named in config | 200 (drop-in) + `MemoryLow` | never touched at runtime; read for PSI only | n/a | inherited (lowest) |
| interactive | `app-slicewise-interactive.slice` | 400 | never braked or clamped | always admit | inherited |
| normal (default) | `app-slicewise-normal.slice` | 100 | braked second | always admit (logged if over budget) | 300 |
| batch ("can wait") | `app-slicewise-batch.slice` | 25 | braked first; CPU clamped | queue if it doesn't fit | 800 |

The pool slice `app-slicewise.slice` (CPU weight 100) sits under `app.slice`. Configs where the orchestrator lives elsewhere can change this; the point is that the pool is a **sibling** of the orchestrator unit. cgroup weights only compete between siblings, so weight 200 against 100 gives the orchestrator at least ⅔ of contended CPU.

Unprivileged processes can only **raise** `oom_score_adj`, so classes only ever go up from the inherited value. The table values are absolute `OOMScoreAdjust=` settings and must stay above the inherited one. A lower request is not an error: systemd accepts it and reports it in `systemctl show`, but the kernel keeps the inherited value (M0). Only `/proc/<pid>/oom_score_adj` shows the value actually applied.

### 5.3 Knobs, and who writes them

**Decision (Claude): systemd is the only writer. The daemon reads cgroupfs directly and writes only through `systemctl --user set-property --runtime`.** systemd owns these files and would overwrite direct writes on reload. The fork/exec cost (about 10 ms) does not matter at the bounded write rate of at most 3 slices × 1 write per 10 s. Every `systemctl` call has a 5 s deadline (spawn plus a `try_wait` loop), so a hung manager cannot freeze the loop.

The only write outside systemd is the wrapper raising its own `oom_score_adj` for scopes. That is procfs, not cgroupfs, and raise-only; the daemon never writes procfs.

| Knob | Set on | Static or dynamic | By |
|---|---|---|---|
| `cpu.weight` | class slices, pool, protected | static | unit files |
| `cpu.max` | pool (`(online − reserve_cpus) × 100 %`, only if `reserve_cpus > 0`; off by default because a pool quota throttles every class, interactive included) | static | unit file |
| `cpu.max` | batch slice | **dynamic** (clamp) | daemon `CPUQuota=` |
| `memory.max` | pool (`pool_memory_max`) | static, **never changed** | unit file |
| `memory.high` | pool (`pool_memory_max − margin`) | static | unit file |
| `memory.high` | batch slice, then normal slice | **dynamic** (brake) | daemon `MemoryHigh=` |
| `memory.low` | protected units | static | drop-in |
| `oom_score_adj`, `memory.oom.group` | each task | launch-time | wrapper (`-p OOMScoreAdjust=` for services; its own `/proc/self/oom_score_adj` before `exec` for `--scope`; `-p OOMPolicy=kill`) |

### 5.4 Control loop

The loop runs every `tick_secs` (default 2). It reads a snapshot, calls `policy::decide(snapshot, state) -> Vec<Action>` (a pure function), applies the actions, then logs them and stores them in a 256-entry ring buffer for `explain`.

**Signals read:**
- `/proc/meminfo` (`MemTotal`, `MemAvailable`).
- For the pool and class slices: `memory.current`, `memory.stat` (`inactive_file`), `cpu.pressure`, `memory.pressure`.
- `cpu.pressure` of the protected units, from the `some avg10` field of each `*.pressure` file.
- `/sys/devices/system/cpu/online`.
- For each reserved unit, `memory.current − inactive_file` of `<pool>/<class slice>/<unit>`, probing the three class slices. ENOENT in all three means the unit is not running; any other read error keeps the last value and logs `event=error`.

**Headroom:**
```
working_set(x) = memory.current(x) − inactive_file(x)          # page cache is reclaimable
H  = min(pool_memory_max − working_set(pool), MemAvailable − system_reserve)
H' = H − Σ outstanding reservations                             # admission only
margin = max(5 % of pool_memory_max, 512 MiB)
```

**Rule M (memory brake).** Engaging the brake is immediate and not rate-limited, because safety comes first. Releasing it is slow.
- If `H < 2 × margin`, set batch `MemoryHigh = working_set(batch) + 64 MiB`. This engages before the pool's static `MemoryHigh` (`pool_memory_max − margin`), so batch is held while there is still room for interactive work. Growth is then throttled, but no memory is reclaimed from under the task.
- If `H < margin`, apply the same to normal.
- When `H > 3 × margin` for 5 consecutive ticks, release one class per 10 s, normal first: `MemoryHigh=infinity`.
- Interactive is never braked. Its only limit is the static pool cap.
- The brake target is set once at engage and not retargeted while held; the skip rule therefore applies only to `CPUQuota`.

**Rule C (batch CPU clamp).**
- When any protected unit's or the interactive slice's `cpu.pressure` some avg10 is ≥ 20 % for 2 ticks, step batch `CPUQuota` down by one CPU's worth per action: `infinity → (C−1)·100 % → … → 100 %`, never below 1 CPU.
- When it is ≤ 5 % for 15 ticks (30 s), step back up by one CPU every 10 s until `infinity`.
- Clamping is fast and releasing is slow. Together with the separate engage and release thresholds, this prevents oscillation.

**Rule A (admission), run on each `admit` request and for the queue head once per tick.**
- `need = mem_hint or class.default_mem`.
- Admit if `H' − need ≥ margin`.
- Otherwise, batch work queues (FIFO, at most one batch admission per tick, queue length capped at 32), or fails fast with `--no-queue`.
- Batch work whose `need + margin` exceeds `pool_memory_max` is rejected, never queued.
- Interactive and normal work is always admitted. If it does not fit, the admission is logged as `over_budget` and Rule M is evaluated at once.
- An admitted unit holds a reservation of `max(0, need − working_set(unit))`. The reservation is dropped when the unit exits or after 300 s, so two quick admissions cannot both claim the same headroom.

**Bounds on all dynamic writes:**
- At most one write per slice per 10 s, except engaging the brake.
- A write is skipped when the change would be under max(5 %, 128 MiB) or under one CPU.
- `MemoryHigh` is never set below the working set.
- `memory.max` is never touched.

### 5.5 Memory headroom versus OOM

Without swap, the allocator cannot get memory back. It can only:

1. avoid admitting work that will not fit (Rule A),
2. throttle the lowest class's growth early (Rule M, which engages at `2 × margin` headroom — before the pool's static `memory.high` at `margin` — M1c review), so the kernel's `memory.high` pressure lands on batch rather than on interactive work, and
3. make the inevitable OOM pick the right victim: `OOMScoreAdjust` ordering by class, plus `memory.oom.group`, so a whole build dies cleanly instead of half of it.

The allocator's own actions never cause an OOM. It never lowers a hard limit, never raises the pool cap and never sets `memory.high` below usage. A remaining OOM means that workloads together exceeded the static pool frame.

### 5.6 The `nproc` / `-j` problem

**Decision (Claude): a launch-time hint, and no CPU quota on tasks.**

```
cpus     = online − reserve_cpus                     (≥ 1)
grant    = max(need, min(H', pool_memory_max))
jobs     = clamp(min(cpus, grant / mem_per_job, --jobs if given), 1, cpus)
```

The wrapper exports `jobs` through `--setenv` for each variable in `jobs_env`; the default is `SLICEWISE_JOBS`, and a config example also adds `CARGO_BUILD_JOBS` and `CMAKE_BUILD_PARALLEL_LEVEL`. Integrations pass `make -j"$SLICEWISE_JOBS"`. Memory, not CPU, is the real reason to lower `-j`: CPU oversubscription is handled by weights.

Live adjustment through a daemon-owned jobserver FIFO is in the backlog (§4).

### 5.7 Fail-safe behaviour

**Decision (Claude): revert to the static defaults.**

- **Daemon crash, kill or stop.** `slicewise.service` has `ExecStopPost=slicewise reset`. `reset` runs `systemctl --user revert` on the three class slices only, never the pool or protected units. It removes the runtime drop-ins and restores the unit-file values, which means no brake and no clamp (verified in M0). `reset` judges success by exit status alone; anything systemctl prints is informational. systemd runs `ExecStopPost` on failure too, then restarts the daemon (`Restart=on-failure`, `RestartSec=5`), which reconciles from cgroupfs. Workloads keep running throughout, and the hard limits never depended on the daemon.
- **No daemon at launch.** The wrapper decides locally with no queue, prints `slicewise: daemon unreachable, launching with static defaults` to stderr, and execs. The orchestrator is never blocked by slicewise being absent.
- **Daemon dies while a wrapper waits in the queue.** The wrapper sees end-of-file and launches with the warning above.
- **Bad config.** The daemon exits with code 78 (`RestartPreventExitStatus=78` stops the restart loop). `ExecStopPost` reverts, wrappers fail open, and `slicewise check-config` shows the error. `run` warns `slicewise: config error, launching with static defaults` and continues on `Config::default()`. `reset` uses the parsed config's `pool_slice` without `validate()` (only the unit-name check), falling back to the default name, so it still reverts.
- **Missing delegation.** At startup the daemon takes the `user@….service` ancestor of its own cgroup (from `/proc/self/cgroup`) as the manager root and checks that its `cgroup.subtree_control` contains `cpu` and `memory`. If there is no such ancestor or a controller is missing, it exits 78 naming the problem.

## 6. Orchestrator interface

### 6.1 CLI

```
slicewise run [--class interactive|normal|batch] [--mem-hint SIZE] [--jobs N] [--no-queue] [--] <systemd-run args…>
slicewise status [--json]
slicewise explain <unit|class> [--json]
slicewise daemon
slicewise reset                 # revert class slices to unit-file values (no daemon needed)
slicewise check-config [PATH]
slicewise units <DIR>           # write the static-frame unit files + protected drop-ins into DIR
```

`run` adds `--user --slice=<class slice> -p OOMPolicy=kill`, one `--setenv=<VAR>=<jobs>` per `jobs_env` entry, and `--unit=slicewise-<class>-<pid>-<n>` if the caller gave no `--unit`. For a service it also adds `-p OOMScoreAdjust=<n>`. Scopes reject that property (M0). So when the `systemd-run` options before the command include `--scope`, `run` instead raises its own `/proc/self/oom_score_adj` to `<n>` before `exec`, and the scope inherits it. It only ever raises; if the write fails it warns on stderr and launches anyway. Everything else is passed to `systemd-run` verbatim as argv, never through a shell. That includes `--wait`, `--pipe`, `--scope` and `-p` properties.

`run` refuses a caller-supplied `--slice`, `--system`, `-M`/`--machine` or `-H`/`--host` with exit code 64; options that take a separate value (e.g. `-p NAME=VALUE`, `-u NAME`) are skipped as pairs when scanning. Its exit code is `systemd-run`'s, through `exec`, except 75 (`EX_TEMPFAIL`) when the daemon does not admit the work (`--no-queue` and it would queue, queue full, or it can never fit), 64 for usage errors and 78 for config errors in `daemon`, `status`, `check-config` and `units`. A caller `--unit` without a suffix gets `.service` or `.scope` in the admission request only; the argv passes it verbatim.

### 6.2 Socket protocol

- **Where:** `$XDG_RUNTIME_DIR/slicewise.sock`, mode 0600. If `XDG_RUNTIME_DIR` is unset, startup fails; there is no `/tmp` fallback. An existing socket owned by another uid, or one that still accepts connections, stops startup; a stale one of ours is removed before binding.
- **Framing:** one request per connection, one JSON object per line (newline-delimited), at most 64 KiB; the whole request line must arrive within 2 s of accept (one overall deadline, not per `read()`). Replies have a 5 s write timeout. Sizes are bytes on the wire; the CLI accepts `3G`.
- **Strictness:** requests use `deny_unknown_fields`, and `v` must be 1.

Requests:
```json
{"v":1,"op":"admit","unit":"slicewise-batch-4711-1.service","class":"batch","mem_hint":3221225472,"jobs_hint":8,"queue":true}
{"v":1,"op":"status"}
{"v":1,"op":"explain","target":"slicewise-batch-4711-1.service"}
```

Responses:
```json
{"v":1,"ok":true,"decision":"admit","jobs":3,"reason":"headroom 4.4G - reserved 0 >= need 3.0G + margin 0.6G"}
{"v":1,"ok":true,"decision":"queued","position":1,"reason":"headroom 1.2G < need 3.0G + margin 0.6G"}
{"v":1,"ok":true,"decision":"reject","reason":"queue full (32)"}
{"v":1,"ok":false,"error":"bad_request","detail":"class: unknown variant 'urgent'"}
```

A `queued` reply is followed on the same connection by exactly one more line, either `admit` or `reject`. Other errors are `unsupported_version`, `too_large`, `timeout` and `busy` (more than 64 open connections).

The `status` and `explain` payloads are §8's data in JSON, documented in `docs/protocol.md`.

### 6.3 Integrating an orchestrator

The orchestrator's task runner replaces its `systemd-run` argv prefix:

```
before: systemd-run --user --slice=tasks.slice -p CPUQuota=… -p MemoryHigh=… <cmd>
after:  slicewise run --class normal --mem-hint 4G -- <cmd>
```

Background commands the orchestrator starts on its own, outside the wrapper, are covered in M2 by `[[adopt]]` patterns. For running units whose names match a pattern, the daemon applies the class's `CPUWeight` and, for batch, includes them in the CPU clamp. Their memory already counts through `MemAvailable`. Adopted units cannot change slice.

Orchestrator-specific glue (runner patch notes, example mappings from task type to class) lives in `integrations/<name>/`, starting with `integrations/hermes/`. Nothing in `src/` knows about any orchestrator.

## 7. Configuration

The config lives at `$XDG_CONFIG_HOME/slicewise/config.toml` (default `~/.config/…`). Every key is optional except `[[protected]]`, which the daemon warns about if it is empty.

```toml
tick_secs       = 2
pool_slice      = "app-slicewise.slice"   # class slices = <pool stem>-<class>.slice
# pool_memory_max = "12G"   # default (unset or 0): (MemTotal - system_reserve) * 3/4; static frame written by slicewise units
system_reserve  = "2G"                    # MemAvailable floor for everything outside the pool
reserve_cpus    = 0                       # > 0: pool CPUQuota = (online - reserve) * 100 %; also throttles interactive (§5.3)
mem_per_job     = "1G"
jobs_env        = ["SLICEWISE_JOBS", "CARGO_BUILD_JOBS", "CMAKE_BUILD_PARALLEL_LEVEL"]

[[protected]]
unit       = "<orchestrator>.service"
cpu_weight = 200
memory_low = "2G"

[class.interactive]
cpu_weight  = 400
default_mem = "2G"

[class.normal]
cpu_weight    = 100
default_mem   = "2G"
oom_score_adj = 300

[class.batch]
cpu_weight    = 25
default_mem   = "2G"
oom_score_adj = 800

[tuning]            # defaults shown; change only with a reason
cpu_psi_engage = 20.0
cpu_psi_release = 5.0
min_write_interval_secs = 10

# M2
[[adopt]]
match = "<orchestrator>-worker-*.scope"   # one '*' wildcard, matched against unit names in app.slice
class = "normal"
```

**Decision (Claude):** use `toml` with `serde` derive and hand-roll nothing for the file format. A strict TOML library with good error spans costs about 6 small transitive crates. A hand-rolled INI subset would be a second, worse format to document. Sizes accept an integer number of bytes or a string with a `K/M/G/T` binary suffix; that parser is about 15 lines in `config.rs` and the CLI uses it too.

Validation after parsing checks:
- weights in 1..=10000,
- sizes no larger than `MemTotal`,
- `pool_memory_max + system_reserve ≤ MemTotal`,
- `pool_memory_max` greater than the margin (so > 512 MiB), so the pool's static `MemoryHigh` is always positive,
- unit names in the systemd character set.

A parse error looks like this:
```
slicewise: config error in /…/config.toml: TOML parse error at line 4, column 19
  |
4 | pool_memory_max = 12G
  |                   ^
string values must be quoted, expected literal string
```
A semantic error is one line, for example `slicewise: config error: class.batch.cpu_weight = 0: must be 1..=10000`. Both exit with code 78.

## 8. Observability

**Logging. Decision (Claude): plain `eprintln!` in logfmt, without the `log` or `tracing` crates.** Under systemd, stderr goes to the journal, which adds timestamps (`journalctl --user -u slicewise`). `SLICEWISE_LOG=debug` adds snapshot lines. Every applied action is one line:

```
level=info event=brake_on target=app-slicewise-batch.slice knob=MemoryHigh from=infinity to=6549012480 reason="pool headroom 498M < margin 614M"
level=info event=admit unit=slicewise-batch-4711-1.service class=batch need=3221225472 jobs=3 reason="headroom 4.4G >= 3.0G + 0.6G"
level=warn event=fallback reason="daemon unreachable" unit=…
```

Event names are `admit`, `queue`, `reject`, `over_budget`, `brake_on`, `brake_off`, `clamp`, `unclamp`, `adopt`, `reset`, `fallback` and `error`. Environment values given to `systemd-run` are never logged, because they may hold secrets.

**`slicewise status`** shows the pool (cap, working set, headroom, margin), one row per class (memory, `MemoryHigh`, `CPUQuota`, CPU and memory PSI, task count, queue length), the protected units' PSI, and the brakes and clamps currently in effect with their age. It exits 3 if the daemon is unreachable, and then prints the cgroupfs view only.

**`slicewise explain <unit|class>`** shows the ring-buffer events for that unit and its class, oldest first, then the current numbers:

```
unit    slicewise-batch-4711-1.service  class=batch  slice=app-slicewise-batch.slice
-4m12s  queued #1: headroom 1.2G - reserved 0 < need 3.0G + margin 0.6G
-2m06s  admitted: headroom 4.4G >= 3.0G + 0.6G; jobs=3 (cpus 5, 3.0G / 1G per job)
-0m41s  batch MemoryHigh infinity -> 6.1G: headroom 0.5G < margin 0.6G
now     memory 5.8G  CPUQuota infinity  cpu psi 12%  mem psi 3%
```

## 9. Security and threat model

- **Trust boundary: the same user.** Anyone who can reach the socket can already run `systemctl --user` themselves, so the socket grants no new privilege. Its socket file is 0600 inside the 0700 `XDG_RUNTIME_DIR`. No peer-credential check is needed, and none is done, so no `unsafe` code is required. Other local users have their own managers and cannot reach the socket. Root is out of scope.
- **Input validation at the socket and CLI:**
  - size limit, read timeout and connection cap (§6.2),
  - closed enums for `op` and `class`,
  - sizes ≤ `MemTotal`, `jobs_hint` ≤ 4096,
  - unit names matching `^[A-Za-z0-9:_.\\@-]{1,255}$` with a valid suffix.
  
  Nothing is ever passed to a shell: `Command` is called with argv only.
- **Blast radius.** The daemon only writes to the three class slices, and in M2 to adopted units matching configured patterns. It never writes to protected units, the pool or anything outside the user manager.
- **Out of scope.** Workloads can move their own processes within the user's delegated subtree, which amounts to a cgroup escape within the same user. slicewise is a resource manager, not a sandbox. The README says so.
- **`unsafe`.** `[lints.rust] unsafe_code = "forbid"`. There are no `unsafe` blocks, and none are planned. Lifting the lint needs approval from the architect and the owner, a `SAFETY:` comment, and a line in this section.
- **Supply chain.** `Cargo.lock` is committed and dependencies are vendored (§11). Any change to `Cargo.lock` gets a separate review.
- **No listener and no telemetry.** There is no outbound network code at all.

## 10. Testing strategy

**Gate for every change:**
```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
CI runs this on each PR. Before merging anything that touches `daemon.rs`, `systemd.rs` or `policy.rs`, run `SLICEWISE_LIVE=1 cargo test -- --ignored` on a host with a user manager. That is done locally by the ops role, because CI runners lack a delegated user manager.

**Unit tests, using `cargo test` with no live system:**
- `policy.rs` is pure. Table tests cover the headroom math, admission (fits, queue, `--no-queue`, `over_budget`, reservation expiry), jobs-hint clamping, and each rule's engage and release thresholds.
- **Invariant loop.** A 20-line LCG feeds 10 000 random snapshot sequences and asserts:
  - `MemoryHigh` is never set below the working set,
  - at most one write per slice per 10 simulated seconds, except engaging the brake,
  - interactive is never braked,
  - the CPU clamp never goes below 100 %,
  - no engage-and-release pair happens within 30 s on an input oscillating around a threshold.
- **Fake cgroupfs.** `cgroup.rs` takes the cgroup root and proc root as paths. Tests build a tempdir tree (`memory.current`, `memory.stat`, `cpu.pressure` in real PSI format, `meminfo`) with a 15-line std-only tempdir helper. `systemd.rs` has a `Record` mode that stores the argv it would run. The tests drive `daemon::tick()` and check the recorded `set-property` calls.
- Parsers: config (including the error text), `parse_size`, PSI lines, protocol request validation, and wrapper argv rewriting (including the refused flags).

**Integration tests (`tests/live_*.rs`).** These are `#[ignore]` and also return early with `SKIP` unless `SLICEWISE_LIVE=1`. They create real transient units with `systemd-run --user` and check, in cgroupfs:
- the slice placement,
- the `CPUQuota` and `MemoryHigh` round-trip and `revert`,
- that `ExecStopPost` reset runs after `kill -9` of the daemon,
- that the env hint is visible inside the unit.

**Load scenario (`tests/load_scenario.rs`, live-gated).** It uses `examples/hog.rs`, which has three modes: `cpu N`, `mem SIZE` and `probe`. `probe` does 1-thread work with 10 ms sleeps and records how late each wake-up is. Sizes scale with the host: `C = online − reserve_cpus`, and `P = min(50 % of MemTotal, 4 GiB)`; the load frame uses the default config except `pool_slice`, `pool_memory_max = P`, `system_reserve` and `tick_secs`. The suite skips unless `MemAvailable ≥ P + 1 GiB`. Pass criteria:

1. **U1.** A lone batch `cpu C` gets ≥ 0.9·C CPUs over 20 s, measured from the `cpu.stat` `usage_usec` delta.
2. **U2.** With `probe` interactive plus two batch `cpu C` hogs, interactive `cpu.pressure` some avg10 is ≤ 10 % within 30 s, the probe's p95 lateness is ≤ 4× its idle baseline, and the batch hogs together still get ≥ 0.5·C.
3. **No oscillation.** At most 6 clamp/unclamp actions in 5 minutes, and none reversed within 30 s.
4. **U3/U4.** Batch `mem 0.6P` is admitted. A second batch `mem 0.6P` is queued, and admitted within 2 ticks of the first one exiting. The `oom_kill` count in every class slice's `memory.events` is 0.
5. **Brake.** Batch starts at 0.45P and keeps growing while interactive grows to 0.3P during the contention. Batch `MemoryHigh` is set, interactive reaches its target with `memory.pressure` some avg10 ≤ 10 %, and interactive is not OOM-killed. After batch exits the brake releases (`brake_off` logged, batch `memory.high` back to `max`). If an OOM does occur, the `oom_kill` is counted in the batch slice only.
6. **Fail-safe.** After `kill -9` of the daemon, the class slices are back to their unit-file values within 10 s, all hogs are still running, and `slicewise run` during the outage launches with the fallback warning.
7. **U5.** The number of `set-property` invocations (counted by a `systemctl` shim on the daemon's `PATH`) equals the number of action log lines.

## 11. Repo layout and dependencies

```
Cargo.toml            # edition 2024, rust-version, [lints] unsafe_code="forbid", clippy all = deny
Cargo.lock            # committed
rust-toolchain.toml   # channel pinned to the stable used at the last release
.cargo/config.toml    # source replacement → third_party/vendor (offline by default)
src/
  main.rs     NAME const, argv dispatch (hand-rolled, ~80 LOC), exit codes
  cli.rs      run (argv rewrite + exec), status, explain, reset, check-config, units (unit-file text)
  config.rs   serde structs, defaults, validation, parse_size
  cgroup.rs   read-only: cgroupfs files, PSI, memory.stat, /proc/meminfo, online CPUs; injectable roots
  systemd.rs  the only process spawner: set-property/revert/show/systemd-run argv; timeout; Real|Record
  policy.rs   pure: Snapshot + State → Vec<Action>; admission; jobs hint (most of the tests)
  daemon.rs   tick loop, rate limiter, ring buffer, socket accept thread, admission queue (Mutex+Condvar)
  proto.rs    Request/Response types, read_one/write_one with size limit
  log.rs      logfmt line helper (~20 LOC)
examples/hog.rs       cpu / mem / probe load generator (tests only, not installed)
tests/                common/mod.rs (tempdir helper), fake_cgroup.rs, live_systemd.rs, load_scenario.rs, m0_live.rs
integrations/hermes/  README.md (runner argv change, task-type→class mapping, adopt pattern example)
integrations/generic/ README.md (any orchestrator / human)
docs/                 design.md (this), protocol.md, operations.md (install, migrate, rollback), m0-findings.md, m1c-load-results.md
.claude/agents/       role definitions (§13); AGENTS.md carries the shared rules for any agent CLI
third_party/vendor/   `cargo vendor` output, committed
third_party/ponytail/ LICENSE for the adapted agent-instruction style
```

The target size is ≤ 2 750 non-blank lines in `src/` outside `#[cfg(test)]` modules at M1 (after `cargo fmt`, counting each file up to its first `#[cfg(test)]`), with a daemon RSS under 10 MB and under 0.5 % of one core at a 2 s tick. M2 may add at most 200. A packet that exceeds its stated budget stops and asks the architect before merging. (Raised from 1 800 at the M1b review: the measured M1a core alone was 1 344.)

| Crate | Version policy | Why (one line) | Weight |
|---|---|---|---|
| `serde` (+`derive`) | `1`, locked | typed config and protocol structs without hand-written mapping | `serde_core` + proc-macro stack (`serde_derive`, `syn`, `quote`, `proc-macro2`, `unicode-ident`), compile-time only |
| `serde_json` | `1`, locked | protocol and `--json` output; the socket is a trust boundary, so no hand-rolled JSON parser | + `itoa`, `zmij`, `memchr` |
| `toml` | current major, `default-features = false`, features `parse`, `serde`, `std` | human-edited config with comments and precise error spans | + `toml_parser`, `toml_datetime`, `serde_spanned`, `winnow` |

That is **3 direct dependencies; 19 lines in `cargo tree -e normal --prefix none | sort -u`** (M1a), with a hard ceiling of 20. `indexmap` and `toml_writer` are in `Cargo.lock` only behind toml's optional `preserve_order` / `display` features, which stay off. The reviewer checks `cargo tree -e normal --prefix none | sort -u | wc -l`. There are no dev-dependencies.

**Rejected:**
- `tokio`: there is no async need with a few connections and one loop.
- `clap`: `run` passes argv through verbatim, which clap fights.
- `libc`/`nix`: std covers everything needed, and they would bring `unsafe`.
- `log`/`tracing`: journald plus logfmt is enough.
- `chrono`: times are relative ages.
- `anyhow`/`thiserror`: one error enum is enough.
- `signal-hook`: `ExecStopPost` does the fail-safe.
- `tempfile`/`proptest`: covered by the 15-line helper and the LCG loop.

**Concurrency. Decision (Claude):** std threads with blocking I/O. One loop thread, one accept thread, one short-lived thread per connection (capped at 64), and shared state in `Arc<Mutex<State>>` with a `Condvar` to wake queued admissions.

**Packaging.**
- `cargo build --release --locked --offline --target x86_64-unknown-linux-musl` (also `aarch64-…-musl`) builds a static binary. It needs `rustup target add` once. A glibc build is fine too.
- Install with `cargo install --path . --locked --offline`, which puts the binary in `~/.cargo/bin`. There is no system-wide install and no root.
- **MSRV:** the latest stable at release time, pinned in `rust-toolchain.toml` and `rust-version`. A bump is its own PR.
- Vendored sources change only in dependency PRs, through `cargo vendor third_party/vendor`.

## 12. Milestones

| Milestone | Scope | Acceptance check |
|---|---|---|
| **M0 spike** (about 150 LOC; only the `systemd` and `cgroup` functions survive) | Unprivileged and live: write the static-frame units; start a transient service and a scope in a class slice; `set-property --runtime` a slice's `CPUQuota` and `MemoryHigh` and see `cpu.max` and `memory.high` change within 1 s; `revert` restores the unit-file values; read `memory.current`, `memory.stat` and `*.pressure`; check that raising `OOMScoreAdjust` works, which property sets `memory.oom.group`, and which properties scopes accept; check that `ExecStopPost` runs after `kill -9`; check that `--setenv` is visible inside the unit; run the musl binary | `SLICEWISE_LIVE=1 cargo test --test m0_live -- --ignored` passes, and `docs/m0-findings.md` records each result in generic terms. Any surprise here changes §5.3 before M1 starts |
| **M1 MVP** | `units`, `daemon` (Rules A, M and C, rate limits, ring buffer), `run` with local fallback, `status`, `explain`, `reset`, `check-config`, `slicewise.service` with `ExecStopPost` | The gate is green, the fake-cgroup tests and the invariant loop pass, and all 7 load-scenario criteria pass on a live host |
| **M2 orchestrator integration** | `integrations/hermes/`: the runner's argv change and task-type→class mapping; `[[adopt]]` for orchestrator-launched background scopes; `docs/operations.md` covering migration and rollback | Tasks from a real orchestrator land in the expected class slices; 7 days of normal use with protected `cpu.pressure` some avg10 p95 ≤ 10 %, 0 `oom_kill` events in the pool for tasks whose hints fit, and every limit change explained; rollback rehearsed once |
| **Backlog** | as listed in §4, each with its trigger | — |

## 13. The agent team for this repo

Rules every role follows:
- **Smallest change that fully works.** Name what you skipped. Never build anything from a later milestone.
- **No host-specific facts in the repo.** No hostnames, usernames, UIDs, real home paths, IPs or real sizes as "the" target. The reviewer greps for these.
- **Gate before "done".** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. Report each command and its result, and say what was not run.
- **Dependencies.** Only the builder edits `Cargo.toml`, and a new or upgraded dependency needs the architect's written approval plus an updated §11 table. `Cargo.lock` and `third_party/vendor/` change only in a dedicated dependency PR.
- **`unsafe`** is forbidden by lint. Lifting the lint needs the architect and the owner (§9).
- **Never silence clippy or skip a failing test.** Live tests never target protected units.
- **Never commit or push unless asked.** Work only on a task branch.
- Every reply ends with `Skipped/Not checked: …` and `Risk: …`.

| Role | Scope (one line) | Owns | Tools |
|---|---|---|---|
| architect | Keeps this design true; decides on protocol, dependencies and `unsafe` | `docs/design.md`, `docs/protocol.md` | Read, Grep, Glob (read-only) |
| builder (the project's delegated coding agent) | Implements the current milestone with the smallest complete diff | `src/`, `examples/`, `Cargo.toml`, `.cargo/`, `rust-toolchain.toml` | Read, Write, Edit, Grep, Glob, Bash |
| tester | Unit, fake-cgroup, live and load-scenario tests; runs the live gate | `tests/` | Read, Write, Edit, Grep, Glob, Bash |
| reviewer | Checks diffs against the design: invariants (§5.4 bounds), logging, the `Cargo.lock` delta, host-fact leaks | — | Read, Grep, Glob (read-only) |
| ops | Unit templates as rendered, install, migration, rollback and live verification on a real user manager | `docs/operations.md`, `integrations/` | Read, Write, Edit, Grep, Glob, Bash |

## 14. Open questions and calls made

**Owner answers (2026-11-06):**
- Q1: background agent tasks (delegated coding/review runs, builds) are **batch**; chat replies and scheduled (cron) jobs are **interactive**.
- Licence: **Apache-2.0 OR MIT** confirmed.
- Design approved as is.

**Open (none of these blocks M0 or M1):**
1. Which of the orchestrator's task types can wait (batch), and which have a human waiting (interactive)? This changes the mapping table in `integrations/hermes/`. The default is that everything is normal.
2. Does the orchestrator retry a task that fails? If it does, daemon-initiated requeueing of preemptible batch work moves up the backlog.
3. The final project name. It is one constant.

**Calls I made (Decision (Claude)):**
- Hybrid approach D, with the static frame shipped first.
- Four classes, one slice per class, with the class read from the slice.
- systemd as the only writer of limits.
- No task CPU quotas: weights plus a launch-time jobs hint.
- Only two dynamic knobs: batch `cpu.max`, and batch/normal `memory.high`.
- No killing in v1.
- Fail-safe means revert to static defaults through `ExecStopPost`.
- Fail-open for launch when the daemon is absent.
- Logfmt through `eprintln!`.
- Three dependencies; threads.
- No pool CPU quota by default (M1c): CPU quota throttling applies to the whole cgroup and stalls interactive work; weights plus Rule C protect the orchestrator.
- Vendored sources committed.
- **License: Apache-2.0 OR MIT**, the Rust ecosystem norm, unless the owner prefers otherwise.

---
