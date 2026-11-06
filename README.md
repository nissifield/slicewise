# slicewise

slicewise is a small, unprivileged Linux resource allocator for machines that run an orchestrator (an AI-agent
gateway, a CI runner) next to its delegated workloads: builds, coding-agent runs, batch jobs. It is one Rust binary.
As a drop-in for `systemd-run` it puts each workload into a class slice under the user's systemd manager, decides
at launch whether the workload fits, and hands it a parallelism hint. As a small daemon it reads PSI pressure and
memory headroom and moves a few cgroup v2 knobs within fixed bounds. Local only: a Unix socket, no network listener,
no telemetry.

**Status: design phase, nothing built.** The approved design is [`docs/design.md`](docs/design.md).

## The problem

- Static caps (`CPUQuota=`, `MemoryMax=`) are sized for the worst case, so idle capacity is wasted.
- Per-task memory caps overlap and still OOM; two heavy jobs admitted together kill each other.
- Nothing tells the kernel "a human is waiting on this" from "this can run overnight", and `-j` is fixed at launch.

## How it works

- **Hybrid.** A static frame of systemd unit files (pool slice, class slices, weights) plus a small daemon that
  reacts within those bounds (§4, §5).
- **Classes.** The orchestrator says `interactive`, `normal` (default) or `batch` ("can wait"); its own unit is
  `protected` and never touched at runtime (§5.2).
- **Admission.** Batch work queues when it would not fit in memory headroom; interactive and normal always start.
- **Memory brake and batch CPU clamp.** Near a full pool, batch growth is throttled first, then normal; under CPU
  pressure on protected or interactive work, batch CPU is stepped down, never below one CPU (§5.4).
- **Launch-time jobs hint.** `SLICEWISE_JOBS` (and friends) sized by CPUs and memory, for `make -j` (§5.6).
- **Fail-safe.** If the daemon dies, class slices revert to unit-file values; with no daemon, launches go ahead on
  static defaults. **No killing in v1** (§5.7).

## Milestones

| Milestone | Scope |
|---|---|
| M0 spike | Prove each systemd/cgroup assumption live, unprivileged; record findings generically |
| M1 MVP | `units`, `daemon` (rules A, M, C), `run` with fallback, `status`, `explain`, `reset`, `check-config` |
| M2 integration | Orchestrator runner change and class mapping, `[[adopt]]`, operations and rollback docs |
| Backlog | Items in design §4, each with its trigger |

## Agent team

Shared rules: [`AGENTS.md`](AGENTS.md).

| Role | Definition |
|---|---|
| architect | [`.claude/agents/sw-architect.md`](.claude/agents/sw-architect.md) |
| builder | [`.claude/agents/sw-builder.md`](.claude/agents/sw-builder.md) |
| tester | [`.claude/agents/sw-tester.md`](.claude/agents/sw-tester.md) |
| reviewer | [`.claude/agents/sw-reviewer.md`](.claude/agents/sw-reviewer.md) |
| ops | [`.claude/agents/sw-ops.md`](.claude/agents/sw-ops.md) |

slicewise is a resource manager, not a sandbox: workloads are trusted at the user's own privilege level.

## Licence

Apache-2.0 OR MIT, at your option: [`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT).

## Credits

Agent instruction style adapted from [ponytail](https://github.com/DietrichGebert/ponytail) by DietrichGebert (MIT);
see [`third_party/ponytail/LICENSE`](third_party/ponytail/LICENSE).
