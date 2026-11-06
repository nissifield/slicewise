# slicewise for any orchestrator, or a human at a shell

**Status: M2 (design §12).** This is the generic interface from design §6; install, migration and rollback are in
`docs/operations.md`. `<orchestrator>.service` is your orchestrator's own unit and `<size>` is a size for your
host.

1. Name your orchestrator's own unit in `[[protected]]` in `$XDG_CONFIG_HOME/slicewise/config.toml`, then run
   `slicewise units <dir>`, review the files, copy them into `$XDG_CONFIG_HOME/systemd/user`, and
   `systemctl --user enable --now slicewise.service`. Protected units are never written at runtime.
2. Wherever you call `systemd-run --user …`, call this instead:

   ```
   slicewise run --class interactive|normal|batch [--mem-hint <size>] [--jobs <n>] [--no-queue] -- <cmd>
   ```

   Drop the caller's `--slice`, `CPUQuota` and `MemoryHigh`; `run` refuses `--slice`, `--system`, `-M` and `-H`
   (exit 64). Everything after `--` is passed to `systemd-run` verbatim.
3. Pick the class by who waits: a human waiting → `interactive`; can run later → `batch`; unsure → `normal`.
   Only `batch` queues.
4. Give `--mem-hint` for anything big; that is what lets admission queue a job instead of OOMing two.
5. Builds use the hint: `make -j"$SLICEWISE_JOBS"`.
6. Add `--no-queue` when the caller holds a lock or a timeout. Exit 75 means "not admitted now, retry later"; any
   other non-zero code is a real error.
7. `slicewise status` shows the pool; `slicewise explain <unit>` shows why a unit got its limits.
8. Units your orchestrator starts outside the wrapper can be adopted in M2:

   ```toml
   [[adopt]]
   match = "<orchestrator>-worker-*.scope"
   class = "batch"
   ```

   The pattern is matched against direct children of the pool's parent slice (`app.slice` by default), contains
   exactly one `*`, and first match wins. Adoption sets the class CPU weight (capped at the pool weight) and, for
   batch, the clamp; it does not move or brake the unit. `slicewise reset` reverts adopted units.

Example from a shell:

```
slicewise run --class batch --mem-hint 3G -- --wait --pipe sh -c 'make -j"$SLICEWISE_JOBS"'
```

Single quotes matter: the variable is set inside the unit, not in your shell.

No daemon running? `run` still launches, on static defaults, with a warning (§5.7).
