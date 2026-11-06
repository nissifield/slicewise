# M1c load scenario results

Design §10 load scenario, criteria 1–7, measured with `examples/hog.rs` and
`tests/load_scenario.rs`. The suite is `#[ignore]` and live-gated.

```
SLICEWISE_LIVE=1 SLICEWISE_LOAD=1 cargo test --offline --locked \
  --test load_scenario -- --ignored --test-threads=1 --nocapture
```

Test parameters: **C = 6** (`online − reserve_cpus`, production default
`reserve_cpus = 0`) and **P = 4 GiB** (`min(50 % of MemTotal, 4 GiB)`); every
size below is a multiple of `C` or `P`. Each criterion runs in its own
test-named static frame (`app-slicewisetest…`, private
`XDG_CONFIG_HOME`/`XDG_RUNTIME_DIR`, rendered unit files, daemon child,
`revert` in `Drop`). Interactive is the test's own interactive class slice.
Whole suite: **523 s** (cap 12 min); final run 7 passed, 0 failed.

| # | Criterion | Pass condition | Measured | Result |
|---|---|---|---|---|
| 1 | U1, lone batch `cpu C` | ≥ 0.9·C over 20 s from `cpu.stat usage_usec` | 5.97 CPUs (1.00·C) | PASS |
| 2 | U2, probe interactive + two batch `cpu C` | interactive `cpu.pressure` some avg10 ≤ 10 %; probe p95 ≤ 4× idle; batch ≥ 0.5·C | psi max 0.00 %; p95 479 µs idle → 62 µs loaded (0.13×); batch 5.97 CPUs (1.00·C); pool throttles 0 | PASS |
| 3 | No oscillation, 5 min | ≤ 6 clamp/unclamp actions; none reversed within 30 s; unclamps = clamps; batch `cpu.max` released | 6 actions (3 clamp, 3 unclamp); shortest reversal 48.0 s; `cpu.max` `max 100000`; 6 `set-property` calls | PASS |
| 4 | U3/U4, two batch `mem 0.6P` | second queues (`event=queue`), is admitted within 2 ticks of the first exiting; 0 `oom_kill` in every class | queued, admitted 0.72 s (0.36 tick) after exit; `oom_kill` 0/0/0 | PASS |
| 5 | Memory brake | batch starts at 0.45P and grows while interactive reaches 0.3P; batch `MemoryHigh` set; interactive `memory.pressure` some avg10 ≤ 10 %; interactive/normal `oom_kill` 0; after batch exits `brake_off` logged and batch `memory.high == max`; `set-property` calls = action lines | batch `MemoryHigh` 2.24 GB (0.52P); interactive 1.29 GB ≥ 0.3P target; interactive mem psi max 0.00 %; `oom_kill` batch 0, interactive 0, normal 0; `brake_off` seen and batch `memory.high` back to `max`; 2 calls = 2 actions | PASS |
| 6 | Fail-safe after `kill -9` | class slices reverted within 10 s; hogs alive; `run` warns and launches | reverted in 0.53 s; hog alive; fallback warning seen; run exited 0 | PASS |
| 7 | U5, explainable | `set-property` calls = action log lines | 2 = 2 (`brake_on`, `brake_off`) | PASS |

All seven `RESULT n PASS` lines were printed in one run; total 7 passed,
0 failed, 523 s. After the run: no `slicewisetest` units, no `slicewise`
runtime drop-ins, no `slicewisetest` unit files, no `slicewise`/`hog`
processes.

## Method

- Criterion 1: `cpu.stat` `usage_usec` delta of the batch class slice over 20 s.
- Criterion 2: `cpu.pressure` of the interactive class slice sampled every
  250 ms over 30 s (max after a 5 s settle); the probe prints p50/p95 wake-up
  lateness; batch usage from the batch `cpu.stat` delta. `pool_throttles` is
  printed for information: the default frame has no pool quota, so it is 0.
- Criterion 3: three 5 s interactive `cpu 2C` bursts, 100 s apart, over the
  5 min window, with a batch `cpu C` hog in the background. Counts come from
  the daemon's `clamp`/`unclamp` lines; the final batch `cpu.max` must be
  `max` and the `set-property` count must equal the action count.
- Criterion 4: cgroup presence through `systemctl show -p ControlGroup`; the
  queue is proven by the daemon's `event=queue` line for the second unit, and
  the admit gap runs from the first unit's cgroup disappearing to the second
  one appearing. The `+ 1 s` over `2 · TICK` covers `systemd-run` start-up,
  not a tick of slack; `oom_kill` from every class slice's `memory.events`.
- Criterion 5: batch starts at 0.45P and keeps growing while interactive
  grows to 0.3P during the contention. Rule M engages at 2·margin headroom,
  before the pool's static `MemoryHigh` (`P − margin` = 0.875P), so the
  combined 0.75P plus batch growth stays below the static high. Counts are read
  once the brake is set and interactive holds its full 0.3P target, and while it
  is still alive. After batch exits, the test waits for the `brake_off` action
  line and for batch `memory.high` to return to `max`.
- Criterion 6: service daemon with `ExecStopPost=slicewise reset`; runtime
  `CPUQuota`/`MemoryHigh` are reverted after `kill -9`; the hog's cgroup stays
  populated; `run` during the outage prints the fallback warning.
- Criterion 7: the daemon's `PATH` points at a generated `systemctl` shim that
  appends every argv and execs the real `systemctl`. This counts actual
  `set-property` invocations (stronger than a debug line) and needs no `src/`
  change, so the 2 750-line budget is untouched.

## Criterion-5 ruling (M1c re-review)

Rule M now engages the batch brake at pool headroom `H < 2·margin`, normal at
`H < margin`, and releases one class per 10 s when `H > 3·margin` for 5
consecutive ticks (was `margin`, `margin/2`, `2·margin`). The earlier threshold
engaged at the same headroom as the pool's own static `MemoryHigh`
(`P − margin`), so the kernel could throttle interactive before the brake
acted. Engaging at `2·margin` leaves room for interactive work, and the
scenario was resized to keep batch + interactive below the static high: batch
starts at 0.45P and grows while interactive grows to 0.3P.

The test also drives the release path live. After batch exits, `brake_off` is
logged and the batch slice's `memory.high` returns to `max`, proving the
`MemoryHigh=infinity` write systemd accepts (the same blind spot that hid the
`CPUQuota=infinity` bug).

## Frame notes

- The rendered unit files go to the **user manager's** unit directory. The
  suite resolves it from the manager's own environment
  (`systemctl --user show-environment`), not from this process's
  `$XDG_CONFIG_HOME`: a harness may set a different one, systemd then creates
  implicit slices, and every rendered property (`CPUWeight`, `MemoryHigh`) is
  silently dropped. `install_units` asserts the batch class weight matches the
  rendered value for exactly that reason.
- `Fixture::new` prints `SKIP` unless `MemAvailable ≥ P + 1 GiB`; the suite
  drives the pool to `P` and must not invite the host's global OOM killer.
- `Drop` stops and reverts every test unit and removes its files on normal
  exit and on panic. A `SIGKILL`/Ctrl-C of the test process skips `Drop`, so
  test-named litter can remain. Manual cleanup (all names contain
  `slicewisetest`):

  ```
  systemctl --user stop '*slicewisetest*'
  systemctl --user reset-failed '*slicewisetest*'
  for unit in $(systemctl --user list-units --all --plain --no-legend \
      'app-slicewisetest*' | cut -d' ' -f1); do
    systemctl --user revert "$unit"
  done
  unit_dir=$(systemctl --user show-environment | sed -n 's/^XDG_CONFIG_HOME=//p')
  rm -f "${unit_dir:-$HOME/.config}/systemd/user/"*slicewisetest*
  rm -rf "$XDG_RUNTIME_DIR"/systemd/user.control/*slicewisetest*
  systemctl --user daemon-reload
  ```

- The frame runs `systemctl --user daemon-reload` after writing the unit files
  and again after removing them. That reloads the whole user manager, so
  unrelated units can see a short reload latency; it is unavoidable because
  the test installs real unit files.

## Test-frame finding

Ruling (architect, M1c review): default `reserve_cpus = 0`; a pool quota
throttles every class. Evidence (the previous frame forced `reserve_cpus = 1`):

```
RESULT 2 FAIL psi_max=8.38% p95_idle_us=477 p95_load_us=9511 batch_cpus=5.00 pool_throttles=310 C=5
```

PSI stayed under 10 %, so Rule C never engaged, but 5 % of probe wake-ups hit
a throttled window and waited ~10 ms: U2's p95 ≤ 4× idle cannot hold under a
pool quota, because CFS bandwidth throttling is cgroup-wide and no class
weight can preempt it. The load frame now runs the production default; the
pool quota itself is still covered by `live_systemd.rs` and the unit-file
tests.

Sizes: `P` is the test pool's `pool_memory_max`, so the kernel enforces it;
the hogs never exceed the multiples above. Criterion 3 alone ran 5 min.
