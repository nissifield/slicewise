# slicewise for Hermes Agent

**Status: M2 (design §12).** This page describes how Hermes Agent, or any task runner built on it, uses slicewise.
Paths are placeholders: `$HERMES_HOME` is the Hermes data directory, `<runner>` is whatever module launches
delegated tasks, and `<orchestrator>.service` is the gateway's own unit. The generic interface is in
`integrations/generic/README.md`; install and rollback are in `docs/operations.md`.

## 1. Protect the gateway

The Hermes gateway is the orchestrator, so it goes in the **protected** class (design §5.2). It is not launched
through `slicewise run`; it is named in `$XDG_CONFIG_HOME/slicewise/config.toml`:

```toml
[[protected]]
unit       = "<orchestrator>.service"
cpu_weight = 200
memory_low = "<size>"   # read the memory_low caveat in docs/operations.md §12
```

`slicewise units <dir>` writes the matching drop-in. slicewise never writes to this unit at runtime (§3). Keep the
gateway's existing memory limits and quota: they are its own hard frame and are not slicewise knobs.

## 2. Change the detached runner's argv prefix

Where `<runner>` builds a `systemd-run` command for a **detached** task, replace the prefix (design §6.3). Before:

```
systemd-run --user --collect --quiet --unit=<unit> --slice=<tasks>.slice \
  -p MemoryHigh=<size> -p MemoryMax=<size> -p CPUQuota=<quota> -p CPUWeight=<weight> <cmd>
```

After:

```
slicewise run --class batch --mem-hint <size> --no-queue -- \
  --collect --quiet --unit=<unit> -p MemoryMax=<size> <cmd>
```

- Drop the caller's `--user` (slicewise adds it), its `--slice` (slicewise adds the class slice and refuses a
  caller `--slice` with exit 64), and its `CPUQuota`, `CPUWeight` and `MemoryHigh` (the class weight, the class
  brake and, for batch, the clamp replace them).
- Keep the hard `MemoryMax`, plus `TasksMax`, `--working-directory` and `--setenv`. Keep `HOME` if the task needs
  it.
- `--no-queue` is required when `<runner>` holds a state lock or a timeout while `systemd-run` runs: without it, a
  queued wrapper blocks until admission and can be killed at the timeout.
- **Exit 75 means "not admitted now, retry later".** Leave the task queued in Hermes; do not mark it failed. A
  later watch or start retries it. Every other non-zero code is a real error.
- `--mem-hint` is what lets admission queue a large task instead of OOMing two. Pass the real expected size.
- If the daemon is down, `run` warns on stderr and launches the task anyway on static defaults; the task is still
  placed in the batch slice.
- Builds read the hint: `make -j"$SLICEWISE_JOBS"` (and the other `jobs_env` names).

Apply the edit atomically (`cp` to a new file, check it compiles, `mv` into place), and keep the previous file as
a dated backup. Running tasks already loaded the old code; new starts read the new file.

**Rollback:** restore the backup and confirm the old `systemd-run` prefix is back. Do this before removing the
slicewise binary (§6).

## 3. Task type → class (owner decision, design §14)

| Hermes task | Class |
|---|---|
| Detached delegated agent tasks (coding and review runs) | `batch` |
| Builds and other work started for a delegated task | `batch` |
| Background commands in worker scopes (see §4) | `batch` |
| Chat replies and relays | left alone |
| Scheduled (cron) jobs | left alone |
| The gateway itself | protected (config, §1) |
| Everything else | `normal` |

A human waiting is `interactive`; work that can run later is `batch`; unsure is `normal`. `interactive` and
`normal` are always admitted; only `batch` queues.

## 4. Background and foreground worker scopes (M2)

Hermes starts background commands, including foreground `run <id>` tasks, in their own worker scopes outside
`slicewise run`. Do **not** route those through `slicewise run`: `--scope` would move the agent out of the worker
scope, and a cancel or a kill of the worker scope would then reach only the runner and could orphan the agent.
Adopt the scope instead, as **batch**:

```toml
[[adopt]]
match = "<orchestrator>-worker-*.scope"
class = "batch"
```

Find the real name before writing the pattern:

```
systemctl --user list-units --type=scope --all --no-legend
systemctl --user show <a-worker-scope> -p Id,ControlGroup
```

Use the stable prefix and suffix, with exactly one `*` where the id varies. The pattern matches unit names that
are direct children of the pool's parent slice (`app.slice` by default), not the units inside the class slices.
First match wins; at most 16 patterns.

Adoption sets the batch weight (25, capped at the pool weight) and includes the scope in the batch CPU clamp. It
does not move the scope, does not brake its memory and does not set `OOMScoreAdjust`. `slicewise reset`, including
the stop path of `slicewise.service`, reverts adopted scopes to their creation values.

**Verification:** the next matching scope shows `CPUWeight=25` within one tick, the journal has an `adopt` line
for it, and `slicewise status` lists it. Non-matching scopes keep `CPUWeight=[not set]`.

**Rollback:** delete the `[[adopt]]` block and `systemctl --user restart slicewise.service`; the stop reverts the
adopted scopes. This restarts slicewise only, never the gateway.

## 5. Leave cron scopes and chat relays alone

Cron scopes (`<orchestrator>-cron-*`) and the chat relay units sit outside the pool and keep their own quotas.
Do not wrap them and do not add adopt patterns for them: queueing a chat reply adds latency for a human, and a
scheduled job should not depend on admission. If one is ever moved inside the pool, use `interactive`, which is
never braked or clamped.

## 6. Rollback order

Restore the runner first, then stop and remove slicewise, and uninstall the binary last:

1. Restore `<runner>` from its backup and verify it calls `systemd-run` again.
2. `systemctl --user disable --now slicewise.service`; confirm it is inactive, the class slices are reverted and
   adopted scopes are back to `CPUWeight=[not set]`.
3. Remove the unit files written by `slicewise units` once no `slicewise-*` units remain, then
   `systemctl --user daemon-reload`.
4. Remove the config and `cargo uninstall slicewise`.

Installing the binary away before step 1 makes every new start raise `FileNotFoundError`. The full procedure,
with verification at each step, is in `docs/operations.md` §10.
