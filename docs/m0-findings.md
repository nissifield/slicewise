# M0 spike findings

Spike scope: design §12, M0. All checks ran as an unprivileged user against a real systemd **user**
manager (`user@$UID.service`), cgroup v2, with `cpu memory pids` delegated to the manager root. Systemd 259,
kernel 7.0. Every test unit, slice and unit file was named with `slicewisetest` and removed afterwards; the
live suite creates nothing else.

Run:

```
SLICEWISE_LIVE=1 cargo test --test m0_live -- --ignored --test-threads=1
```

The spike has **no dependencies**, so `Cargo.lock` has no package entries and `third_party/vendor/` plus
`cargo vendor` are deferred to the first dependency PR (§11). `src/` holds only `main.rs` (the `NAME` constant and
a version stub), `lib.rs` (two module declarations), `systemd.rs` (the only spawner) and `cgroup.rs` (read-only,
injectable roots, with parser unit tests).

## Results

| # | Assumption | Result | Evidence (generic) | Impact on design §5.3 / §5.7 |
|---|---|---|---|---|
| 1 | Static-frame pool and class slice unit files start and nest | **PASS** | Wrote `app-slicewisetest<PID>x<N>.slice` (`CPUWeight=100`) and `app-slicewisetest<PID>x<N>-batch.slice` (`CPUWeight=25`, `CPUQuota=100%`, `MemoryHigh=128M`) into `$XDG_CONFIG_HOME/systemd/user/` (default `~/.config/systemd/user/`), then `systemctl --user daemon-reload` and `systemctl --user start <pool> <class>`. `systemctl --user show -p ControlGroup --value <class>` returned `<user manager>/app.slice/<pool>/<class>`; a probe service's `/proc/<pid>/cgroup` was `<class>/<unit>.service`. The dash nests the class under the pool exactly as wanted. | None. The static frame needs only unit files plus a reload; the class can be read from the unit's parent slice. |
| 2 | A transient service and a scope land in the class slice | **PASS** | Service: `systemd-run --user --unit=<n> --slice=<class> -p TasksMax=64 -- sleep 30`; its `MainPID` had `/proc/<pid>/cgroup` under `<class>/`. Scope: `systemd-run --user --scope --unit=<n> --slice=<class> -p TasksMax=64 -- sleep 5`; the scope's `cgroup.procs` process was under `<class>/<unit>.scope`. | None. `--slice=` is enough for both unit types; the wrapper's argv passthrough plan holds. |
| 3 | `set-property --runtime` reaches `cpu.max` / `memory.high` within 1 s | **PASS** | `systemctl --user set-property --runtime <class> CPUQuota=150% MemoryHigh=256M` produced `cpu.max=150000 100000` and `memory.high=268435456`. Observed latency **10 ms** (5 ms polls, measured from before the command to the first matching read). | Confirms the §5.3 write path and that the 5 s per-call deadline is ample. |
| 4 | Revert restores the unit-file values | **PASS** | After #3, `systemctl --user revert <class>` restored `cpu.max=100000 100000` and `memory.high=134217728` (the unit-file values) with no further write. `revert` exits 0; success is judged by exit status only. The `set-property --runtime` back-to-defaults fallback was not needed. | §5.7 can use `systemctl --user revert <class>` as the fail-safe; `reset` judges success by exit status alone. |
| 5 | Slice and host signals are readable unprivileged | **PASS** | Read `<class>/memory.current` (non-zero for an idle `sleep`), `<class>/memory.stat` (`anon`, `file`, `inactive_file` parsed), `<class>/cpu.pressure`, `<class>/memory.pressure` (`some avg10`, and `full avg10` where present), `/proc/meminfo` `MemAvailable` (positive) and `/sys/devices/system/cpu/online` (parsed to a count). No privileged access needed. | None. Every §5.4 signal is available from cgroupfs/procfs. `full` is optional in the parser because some kernels omit it. |
| 6 | Raising `OOMScoreAdjust` works; lowering below the inherited value fails | **SURPRISE** | *Observed in RESULT output; the lowering result and scope rejection are not asserted yet (M1 adds assertions).* `-p OOMScoreAdjust=800` on a transient service gave `/proc/<pid>/oom_score_adj=800`. Requesting `-p OOMScoreAdjust=-100` was **accepted** (`systemd-run` exit 0, unit property `OOMScoreAdjust=-100`) but the process kept its inherited kernel value `100`; there was **no error**. The lowering therefore has no effect rather than failing loudly. | Raising +300/+800 is safe (§5.2 ordering works). A negative adjust cannot be relied on and `systemctl show` is not proof of the kernel value; if needed, read `/proc/<pid>/oom_score_adj`. Classes only ever raise, as designed. |
| 7 | `OOMPolicy=kill` sets `memory.oom.group`; scope property support | **SURPRISE** | *Scope accept/reject list observed in RESULT output, not asserted; `memory.oom.group` for scopes unverified.* Service with `-p OOMPolicy=kill` had `memory.oom.group=1`. A scope accepted `OOMPolicy=kill`, `MemoryHigh=64M` and `CPUWeight=50`; it rejected `OOMScoreAdjust=800` with exit 1 and the verbatim message `Unknown assignment: OOMScoreAdjust=800`. | `memory.oom.group` is set by `OOMPolicy=kill` for services (and accepted for scopes). But §6.1's `run` always adds `-p OOMScoreAdjust=<n>`, so any `slicewise run … -- --scope` would fail. See proposed change 1. |
| 8 | `ExecStopPost=` runs after the main process is `kill -9`'d | **PASS** | Transient service with `-p ExecStopPost=/usr/bin/touch <tmpdir>/stop-post-marker`; `systemctl --user kill --signal=SIGKILL --kill-whom=main <unit>`; the marker appeared after **15–30 ms** and the unit ended `failed`. | §5.7/§10 fail-safe is confirmed: `ExecStopPost` runs on SIGKILL, so `slicewise reset` can revert after a daemon crash. |
| 9 | `--setenv=` is visible inside the unit | **PASS** | `systemd-run --user --unit=<n> --slice=<class> --setenv=SLICEWISE_JOBS=3 --wait --pipe /usr/bin/printenv SLICEWISE_JOBS` printed `3`. systemd-run's own status summary went to stderr. | §5.6 job hint works. The wrapper `exec`s systemd-run and must not parse its output. |
| 10 | The musl release binary builds and runs | **PASS** | `cargo build --release --target x86_64-unknown-linux-musl` succeeded; `file` reported `ELF 64-bit LSB pie executable, x86-64, static-pie linked`; running it printed `slicewise 0.1.0`. | None. Static packaging without root works. |

## Notes

- **Runtime drop-ins live in the user runtime dir, not the repo.** `revert` removes files such as
  `<runtime-dir>/systemd/user.control/<unit>.d/50-CPUQuota.conf`; nothing was written outside the unit
  directory, the runtime dir and tempdirs.
- **Slice names must avoid extra dashes in the test stem.** An early test used `app-slicewisetest<PID>-<N>.slice`,
  which auto-created an ancestor `app-slicewisetest<PID>.slice` that outlived the test. The suite now uses no dash
  in the stem and calls `reset-failed` on its units during cleanup. Production slice names are fixed by design and
  are not affected.
- **Cleanup passes:** after the live run, `systemctl --user list-units 'app-slicewisetest*' --all` and the
  unit-file `grep` both return nothing.
- **Timing is host-dependent.** Latencies above are single-run observations, not guarantees; the 1 s budget held
  with a wide margin.
- No test wrote to `app.slice`, `session.slice`, the orchestrator's units or any other pre-existing unit. Every
  `systemctl` call that names a unit (`start`, `stop`, `kill`, `set-property`, `revert`, `reset-failed`) goes
  through a helper that refuses argv without `slicewisetest`. `systemd-run` calls and unit-file writes and
  removals use fixture names built from the same mark, but are not asserted. `systemctl --user daemon-reload`
  is global and unguarded: it also loads any unit-file edits the user had not yet reloaded, so run the live
  suite only when no protected unit shows `NeedDaemonReload=yes`.
- **Manual cleanup after a hard kill** (Ctrl-C/SIGKILL skip `Drop`): `systemctl --user stop '*slicewisetest*'`,
  `systemctl --user reset-failed '*slicewisetest*'`, remove `app-slicewisetest*.slice` from the user unit
  directory, then `systemctl --user daemon-reload`.

## Proposed design changes (for the architect; `docs/design.md` was not edited)

1. **§6.1 / §5.3: do not pass `OOMScoreAdjust` to scopes.** systemd rejects the property on `--scope`
   (`Unknown assignment: OOMScoreAdjust=800`), so `run` cannot unconditionally add
   `-p OOMScoreAdjust=<n>` when the caller asked for a scope. Options: (a) detect `--scope` in the passthrough
   argv and raise the wrapper's own `/proc/self/oom_score_adj` before `systemd-run --scope` (the scope then
   inherits it), or (b) omit the property for scopes and document that class OOM ordering does not apply to
   them. Option (a) keeps the §5.2 guarantee and stays raise-only; it writes procfs rather than cgroupfs, so it
   needs an explicit §5.3 note.
2. **§5.2: state that a rejected negative `OOMScoreAdjust` may be silently ignored, not an error.** Do not use
   `systemctl show -p OOMScoreAdjust` as evidence of the applied value; read `/proc/<pid>/oom_score_adj` if the
   daemon ever needs to verify.
3. **§5.7: `revert` is the fail-safe primitive and it writes to stdout.** `slicewise reset` should treat
   non-empty stdout as informational, and should only ever name the class slices (never the pool).

**Architect rulings (review of the spike):** change 1 PARTIAL (option a, raise-only, fail-open, in `cli.rs`; M1 live test must assert scope inheritance and check `memory.oom.group` for scopes); change 2 ACCEPT; change 3 PARTIAL (no stdout premise). Applied to `docs/design.md` §5.2, §5.3, §5.7, §6.1.

## M2 adopt spike

Question: does `systemctl --user revert` on a **transient** scope restore the values it was created with and leave
the scope running? The M2 `[[adopt]]` reset path needs both.

Probe: two throwaway scopes named `sw-adopt-probe-<n>.scope`, created and stopped only for this spike. No other
unit was read or written. Values come from the scope's `ControlGroup` in cgroupfs (`cpu.weight`, `cpu.max`), run as
the unprivileged user against `user@$UID.service` (default `app.slice`). Both scopes stayed active throughout and
were stopped afterwards.

**(a) set-property reaches a transient scope — PASS**

```
systemd-run --user --scope --unit=sw-adopt-probe-1 sleep 120 &
systemctl --user set-property --runtime sw-adopt-probe-1.scope CPUWeight=25 CPUQuota=300%
```

Before: `cpu.weight=100`, `cpu.max=max 100000`. `set-property` exit 0; after: `cpu.weight=25`,
`cpu.max=300000 100000`; scope active.

**(b) revert restores the creation values — PASS**

```
systemctl --user revert sw-adopt-probe-1.scope
```

`revert` exit 0; scope still active; `cpu.weight=100`, `cpu.max=max 100000`. `revert` removed the runtime
drop-ins `50-CPUWeight.conf` and `50-CPUQuota.conf` under `<runtime-dir>/systemd/transient/<scope>.d`.

**(c) revert restores a creation-time quota — PASS**

```
systemd-run --user --scope --unit=sw-adopt-probe-2 -p CPUQuota=150% sleep 120 &
systemctl --user set-property --runtime sw-adopt-probe-2.scope CPUWeight=25 CPUQuota=300%
systemctl --user revert sw-adopt-probe-2.scope
```

Created with `cpu.weight=100`, `cpu.max=150000 100000`; after `set-property`, `25` / `300000 100000`; after
`revert`, exit 0, scope active, `cpu.weight=100`, `cpu.max=150000 100000`.

**Ruling: the main ruling applies.** (b) and (c) both pass, so `reset` may `systemctl --user revert` adopted
units, P4 keeps first-sight adoption, and the section 2 fallback (write `CPUQuota=`, refuse pre-throttled units,
document the pattern caveat) is not needed.

Cleanup: both probe scopes stopped;
`systemctl --user list-units 'sw-adopt-probe*' --all --no-legend | wc -l` → `0` and
`ls "$XDG_RUNTIME_DIR/systemd/user.control" | grep -c sw-adopt` → `0` (no runtime drop-ins left).

