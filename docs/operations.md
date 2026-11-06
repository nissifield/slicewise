# slicewise operations

Install, migrate and roll back slicewise on one unprivileged user's systemd manager. `docs/design.md` is the source
of truth; this page is the runbook for it (M2, design §5.7 / §6 / §7 / §12).

Everything runs through the **user** manager (`user@$UID.service`). No step needs root or `sudo`. Placeholders:
`<orchestrator>.service` is the unit that runs your orchestrator, `<runner>` is whatever code launches its tasks,
`<size>` is a size for your host (`2G`, `512M`, or bytes), and `<dir>` is a scratch directory.

## 1. Preconditions

You need cgroup v2 and a user manager with the `cpu` and `memory` controllers delegated. Check the manager root,
which is the `user@$UID.service` ancestor of your session:

```
stat -fc %T /sys/fs/cgroup
cat /sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/cgroup.subtree_control
```

**Expected:** `cgroup2fs`, then a line containing `cpu` and `memory` (other controllers may appear).

If the controllers are missing, slicewise's daemon exits 78 naming the problem, wrappers still launch on static
defaults, and no class limits are enforced. Delegation is set by the administrator on `user@$UID.service`; this is
the only step that may need root, so stop and report instead of running it yourself.

**Rollback:** none; these checks are read-only.

## 2. Install

```
cargo install --path . --locked --offline
slicewise --version
```

**Expected:** `cargo install` writes the binary to `$CARGO_HOME/bin` (default `~/.cargo/bin`), and `--version`
prints `slicewise <version>`.

The binary is self-contained; there is no system-wide install. Do not uninstall an older binary until the runner
has been moved off it (§10).

**Rollback:** `cargo uninstall slicewise`. If the old runner was already migrated, restore it first (§10).

## 3. Configure and check

The config is `$XDG_CONFIG_HOME/slicewise/config.toml`, defaulting to `~/.config/slicewise/config.toml`. Every key
is optional; the daemon warns when `[[protected]]` is empty. Start from the defaults and set only what your host
needs:

```toml
tick_secs       = 2
system_reserve  = "<size>"        # MemAvailable floor for everything outside the pool
mem_per_job     = "<size>"        # one job's share when no --jobs is given
# pool_memory_max = "<size>"      # unset or 0: (MemTotal - system_reserve) * 3/4

[[protected]]
unit       = "<orchestrator>.service"
cpu_weight = 200
memory_low = "<size>"             # read the caveat in §12 before relying on this

[class.batch]
default_mem = "<size>"            # used when a task has no --mem-hint

# M2: units the orchestrator starts outside `slicewise run`
[[adopt]]
match = "<orchestrator>-worker-*.scope"
class = "batch"
```

Then:

```
slicewise check-config
```

**Expected:** exit 0 and a pretty-printed dump of the resolved config, including `pool_memory_max: Size(...)`,
`system_reserve: Size(...)`, the protected unit and the adopt pattern. A parse error names the file, line and
column; a semantic error is one line (for example `class.batch.cpu_weight = 0: must be 1..=10000`). Both exit 78.

Do not trust the dump for the pool cap alone: `pool_memory_max` is resolved from `MemTotal` when it is unset.

**Rollback:** restore your previous config, or `rm -r $XDG_CONFIG_HOME/slicewise`. A running daemon keeps the
config it loaded; restart `slicewise.service` to pick a change.

## 4. Install the static frame

`slicewise units` only writes text. Stage it, review it, then copy without overwriting anything that already
exists:

```
D=$(mktemp -d)
slicewise units "$D"
grep -r . "$D"
```

**Expected:** one path for the pool slice, one per class slice (interactive, normal, batch), one for
`slicewise.service`, and one drop-in per `[[protected]]` entry — five files plus the protected entries. In the
staged text:

- the pool slice has `MemoryMax=` and `MemoryHigh=` and no `CPUQuota=` unless `reserve_cpus > 0`;
- the class slices have `CPUWeight=400`, `100` and `25` (or your overrides);
- `slicewise.service` has `ExecStart=<binary> daemon`, `ExecStopPost=<binary> reset`,
  `RestartPreventExitStatus=78` and `MemoryMax=64M`;
- each protected drop-in has `CPUWeight=` and `MemoryLow=`.

Check for collisions before copying:

```
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
for f in app-slicewise.slice app-slicewise-interactive.slice app-slicewise-normal.slice \
         app-slicewise-batch.slice slicewise.service "<orchestrator>.service.d/10-slicewise.conf"; do
  test -e "$UNIT_DIR/$f" && echo "EXISTS $f"
done
```

**Expected:** no output. If a file exists, stop and reconcile it by hand; `cp -n` below would silently keep the
old one.

```
cp -rn "$D"/. "$UNIT_DIR"/
systemctl --user daemon-reload
systemctl --user enable --now slicewise.service
```

**Expected:** `systemctl --user is-active slicewise` prints `active`; `journalctl --user -u slicewise -n 30
--no-pager -o cat` has no `event=error`; `slicewise status` shows the pool cap and margin, the class rows, and the
protected unit's PSI; `systemctl --user show slicewise -p MemoryCurrent` is well under the 64M cap.

The protected drop-in applies on reload. If `systemctl --user show <orchestrator>.service -p MemoryLow` did not
change, leave it: never restart the orchestrator just for this (see §12).

**Rollback:** stop the daemon and remove only the files this step wrote, then reload. See §10 step 3.

## 5. Smoke runs

No agent and no runner change yet. Check placement, the jobs hint and the OOM order:

```
slicewise run --class batch --mem-hint 1G -- --wait --pipe --quiet /bin/sh -c \
  'grep -o "app-slicewise.*" /proc/self/cgroup; echo jobs=$SLICEWISE_JOBS; cat /proc/self/oom_score_adj'
```

**Expected:** a control group under `app-slicewise.slice/app-slicewise-batch.slice/`, `jobs=` at most the online
CPU count, and `800`.

The same with `--class interactive` lands in the interactive slice and its `/proc/self/oom_score_adj` is the
inherited value (interactive has no override).

Check that admission fails fast for impossible work:

```
slicewise run --class batch --mem-hint <size-near-the-pool-cap> --no-queue -- --wait /bin/true; echo rc=$?
```

**Expected:** a `can never fit` reason on stderr and `rc=75`. Without `--no-queue` the same request queues until
the daemon admits it or the pool changes.

`slicewise explain batch` then shows the admit and reject events with their reasons.

**Rollback:** none; these are transient units and they are gone when the commands finish.

## 6. Move a runner from `systemd-run`

`slicewise run` replaces the runner's `systemd-run` prefix; everything after `--` is passed to `systemd-run`
verbatim, never through a shell.

Before:

```
systemd-run --user --collect --quiet --unit=<unit> --slice=<tasks>.slice \
  -p MemoryHigh=<size> -p MemoryMax=<size> -p CPUQuota=<quota> -p CPUWeight=<weight> <cmd>
```

After:

```
slicewise run --class batch --mem-hint <size> [--jobs <n>] [--no-queue] -- \
  --collect --quiet --unit=<unit> -p MemoryMax=<size> <cmd>
```

What changes, and why:

- Drop the caller's `--user`, `--slice`, `CPUQuota` and `MemoryHigh`. `run` adds `--user` and the class slice,
  and refuses a caller `--slice` (exit 64). Limits come from the class; a per-unit `MemoryHigh` would throttle the
  task even with free room.
- Keep the hard `MemoryMax` if the runner sets one, plus `TasksMax`, `--working-directory` and `--setenv`.
- Add `--no-queue` only when the runner holds a lock or a timeout while `systemd-run` runs. Without it, a queued
  wrapper blocks until the daemon admits the task; with it, the call returns **exit 75 = not admitted now, retry
  later**, which the caller must map to "leave the task queued", not "failed".
- Omit `--jobs` to let slicewise compute the hint; builds read `$SLICEWISE_JOBS` (and the other `jobs_env` names,
  for example `make -j"$SLICEWISE_JOBS"`).
- If the daemon is down, `run` prints `slicewise: daemon unreachable, launching with static defaults`, launches
  anyway and does not queue (§9).

Use the class for intent: a human waiting → `interactive`; can run later → `batch`; unsure → `normal`.

**Verification:** start one real task, then

```
systemctl --user show <unit> -p ControlGroup
journalctl --user -u slicewise -o cat | grep 'event=admit.*<unit>'
slicewise explain <unit>
```

**Expected:** `ControlGroup` is under the class slice, the journal line says `class=batch` (or the class you
chose), and `explain` shows the admit reason and the computed jobs. A task that returns 75 must still be queued in
the runner, not marked failed.

**Rollback:** restore the runner's previous file from your backup and confirm the old `systemd-run` prefix is back.
Do this before uninstalling the binary (§10).

## 7. Adopt units the orchestrator starts itself (M2)

Background commands the orchestrator starts outside `slicewise run` keep their own unit. `[[adopt]]` gives them a
class's CPU weight and, for batch, includes them in the CPU clamp. They stay in place; adopted units cannot change
slice.

Find the real scope name first:

```
systemctl --user list-units --type=scope --all --no-legend
systemctl --user show <a-scope> -p Id,ControlGroup
```

Pick the stable prefix and suffix and put exactly one `*` where the id varies:

```toml
[[adopt]]
match = "<orchestrator>-worker-*.scope"
class = "batch"
```

The interface, as implemented in M2:

- `match` is matched against unit names that are **direct children of the pool's parent slice** (`app.slice` by
  default), i.e. the protected unit's siblings. It contains exactly one `*`; the rest is in the systemd unit
  character set and the pattern ends in `.scope` or `.service`.
- At most 16 patterns; the first match wins. A pattern that matches a `[[protected]]` unit is a config error
  (exit 78).
- The weight is the class `CPUWeight`, capped at the pool slice's weight (100), so interactive and normal adopt as
  100 and batch as 25. An adopted unit gets that weight on first sight, plus the current batch clamp.
- Batch clamp values are written to every adopted batch unit, one logged action each (U5: one `set-property` per
  action line).
- At most 32 adopted units are tracked; the rest are ignored and logged once with `event=error`. There is no
  memory brake and no `OOMScoreAdjust` for adopted units — their memory only counts through `MemAvailable`.
- `slicewise reset` (and `ExecStopPost` on a daemon stop) reverts every present unit that matches a pattern to its
  creation values.

**Verification:** after the next matching unit starts, within one tick

```
systemctl --user show '<orchestrator>-worker-*' -p Id,CPUWeight
journalctl --user -u slicewise -o cat | grep 'event=adopt'
slicewise status
```

**Expected:** each matching scope shows `CPUWeight=25` (batch) or `100`; there is one `adopt` line per unit; and
`status` lists the adopted units (M2). Non-matching scopes keep `CPUWeight=[not set]`.

**Rollback:** delete the `[[adopt]]` block and `systemctl --user restart slicewise.service`. The stop runs
`reset`, which reverts adopted units, and the start reconciles from cgroupfs. This restarts slicewise only, never
the orchestrator.

## 8. Day to day

- `slicewise status` shows the pool (cap, working set, headroom, margin), one row per class, the protected units'
  CPU PSI, the queue and active brakes/clamps. `--json` is the same data for tooling. It exits 3 when the daemon
  is unreachable and then prints the cgroupfs view only.
- `slicewise explain <unit|class>` replays the ring buffer for that unit and its class, then shows the current
  numbers.
- The journal is logfmt: `level=info event=brake_on target=… knob=MemoryHigh from=infinity to=… reason="…"`.
  Event names are `admit`, `queue`, `reject`, `over_budget`, `brake_on`, `brake_off`, `clamp`, `unclamp`,
  `adopt`, `reset`, `fallback` and `error`. Environment values are never logged.

## 9. Fail-safe behaviour

| Situation | What the operator sees | What happens to work |
|---|---|---|
| Daemon stopped, crashed or killed | `slicewise.service` restarts after 5 s; the journal shows `reset` | `ExecStopPost=reset` reverts the class slices and adopted units; running work continues; hard limits never depended on the daemon |
| No daemon at launch | `slicewise: daemon unreachable, launching with static defaults` on stderr | The task launches anyway, in its class slice, with static limits and no admission |
| Daemon dies while a wrapper waits in the queue | The same warning; the wrapper sees end-of-file | The task launches with static defaults |
| Bad config | `slicewise check-config` exits 78; the daemon exits 78 and does not restart (`RestartPreventExitStatus=78`) | Wrappers warn and launch on defaults; `reset` still reverts using the default pool name |
| Missing delegation | Daemon exits 78 naming the controller | Wrappers launch on defaults; no class limits |
| More than 32 adopted units | One `event=error` | Extra units are left untouched |
| An adopted unit exits | It disappears from `status` | No write, no error |

## 10. Rollback

Do this in order. The runner goes first, the binary last: a runner that still calls `slicewise` after uninstall
raises `FileNotFoundError` and fails every start.

1. **Restore the runner.** Put back the previous runner file (or reverse the edit) so it calls `systemd-run`
   again. Verify the old prefix is present and one start lands in its old slice.
2. **Stop slicewise.** `systemctl --user disable --now slicewise.service`. Verify `is-active` is `inactive`, the
   class slices are back to their unit-file values (`systemctl --user show app-slicewise-batch.slice -p
   CPUQuotaPerSecUSec,MemoryHigh` shows `infinity` for both) and adopted units show `CPUWeight=[not set]`.
   Stopping runs `reset`; `slicewise reset` alone is also safe and does not stop workloads.
3. **Remove the frame.** Once `systemctl --user list-units 'slicewise-*' --all` shows nothing in the pool, remove
   the files from §4 (`app-slicewise.slice`, the three class slices, `slicewise.service`, and each protected
   drop-in) from `$UNIT_DIR`, then `systemctl --user daemon-reload`. Verify the orchestrator's `MemoryLow` is
   back to its previous value, its `MainPID` is unchanged, and `DropInPaths` no longer lists `10-slicewise.conf`.
4. **Remove the config and binary.** `rm -r $XDG_CONFIG_HOME/slicewise` and `cargo uninstall slicewise`, only
   after step 1.

Rehearse this once per milestone that changes units or config, and say when it was last rehearsed.

## 11. Soak sampler (generic)

For the 7-day M2 soak, sample once a minute into a small CSV. Use a user timer and a `Type=oneshot` service; keep
the script in `~/.local/bin` and the CSV under `$XDG_STATE_HOME/slicewise-soak.csv`
(default `~/.local/state`).

Script `~/.local/bin/slicewise-soak`:

```sh
#!/bin/sh
set -eu
R=/sys/fs/cgroup
cg() { systemctl --user show -p ControlGroup --value "$1" 2>/dev/null || true; }
at() { cg=$(cg "$1"); if [ -n "$cg" ]; then printf '%s%s' "$R" "$cg"; fi; }
read_() { p=$(at "$1"); [ -n "$p" ] || return 0; cat "$p/$2" 2>/dev/null || true; }
num() { p=$(at "$1"); [ -n "$p" ] || return 0; sed -n "s/^$2 \([0-9]*\)$/\1/p" "$p/$3" 2>/dev/null || true; }
psi() { p=$(at "$1"); [ -n "$p" ] || return 0; sed -n "s/^some .*$2=\([0-9.]*\).*/\1/p" "$p/cpu.pressure" 2>/dev/null || true; }
printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
  "$(date +%s)" \
  "$(psi "<orchestrator>.service" avg10)" "$(psi "<orchestrator>.service" total)" \
  "$(psi app-slicewise-interactive.slice avg10)" "$(psi app-slicewise-interactive.slice total)" \
  "$(num app-slicewise.slice oom_kill memory.events)" \
  "$(read_ app-slicewise.slice memory.current)" \
  "$(awk '/^MemAvailable:/{print $2*1024}' /proc/meminfo)" \
  "$(read_ app-slicewise-batch.slice memory.high)" \
  "$(read_ app-slicewise-batch.slice cpu.max | tr ' ' '/')" \
  "$(systemctl --user is-active slicewise.service)" \
  >> "${XDG_STATE_HOME:-$HOME/.local/state}/slicewise-soak.csv"
```

Columns: epoch; protected CPU PSI some avg10 and `total`; interactive slice avg10 and `total`; pool `oom_kill`;
pool `memory.current`; `MemAvailable`; batch `memory.high`; batch `cpu.max`; daemon state. Empty fields mean the
unit was absent at that sample.

`~/.config/systemd/user/slicewise-soak.service`:

```ini
[Unit]
Description=slicewise soak sampler

[Service]
Type=oneshot
ExecStart=%h/.local/bin/slicewise-soak
```

`~/.config/systemd/user/slicewise-soak.timer`:

```ini
[Unit]
Description=slicewise soak sampler timer

[Timer]
OnActiveSec=60
OnUnitActiveSec=60
AccuracySec=5

[Install]
WantedBy=timers.target
```

```
chmod +x ~/.local/bin/slicewise-soak
systemctl --user daemon-reload
systemctl --user enable --now slicewise-soak.timer
```

**Verification:** `systemctl --user list-timers slicewise-soak.timer` shows it next elapsing within a minute, and
after two minutes `tail -n 3 "${XDG_STATE_HOME:-$HOME/.local/state}/slicewise-soak.csv"` has three lines about
60 s apart.

**Day-7 checks:**

- Protected CPU PSI p95 ≤ 10 %:
  `cut -d, -f2 <csv> | sort -n | awk '{a[NR]=$1} END{print a[int(NR*0.95)]}'`.
- No pool OOM kill: `cut -d, -f6 <csv> | sort -u` prints only `0`.
- Every limit change explained:
  `journalctl --user -u slicewise --since -7d -o cat | grep -E 'event=(brake_on|brake_off|clamp|unclamp|adopt)' | grep -vc reason=` prints `0`.
- Placement: every admit for a runner unit says `class=batch`, and every matching background scope has an
  `adopt` line.
- Availability: the daemon column is `active` for essentially every sample; gaps explain any missing metrics.

**Rollback:** `systemctl --user disable --now slicewise-soak.timer`, remove the two unit files, then
`systemctl --user daemon-reload`. Keep the CSV as the soak record.

## 12. Caveats

- **`memory_low` on a protected unit usually has no effect.** `MemoryLow` protects a cgroup against reclaim
  pressure from its *parent* only when protection is passed down (`memory_recursiveprot`) and the ancestors carry
  `memory.low` too. On most hosts `user.slice`, `user-$UID.slice`, `user@$UID.service` and `app.slice` all have
  `memory.low=0`, and `memory_recursiveprot` passes protection down, not up. Setting the ancestors needs root, so
  treat `memory_low` as documentation of intent rather than a guarantee; the protected `CPUWeight` still works.
  Check with `cat /sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/app.slice/memory.low` and the same
  on each ancestor. Never restart the orchestrator for this.
- **The clamp fans out per unit.** When batch is clamped to N CPUs, each adopted batch scope gets N CPUs, not N
  in total. Watch the soak before assuming a clamp fixed contention.
- **Adopted units keep their own memory and OOM behaviour.** slicewise does not brake them and cannot lower their
  `OOMScoreAdjust`; only a unit's own `MemoryMax` or its creator can bound that.
