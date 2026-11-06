# slicewise for any orchestrator, or a human at a shell

**Status: nothing exists yet; this is the M1/M2 interface from design §6.**

1. Name your orchestrator's own unit in `[[protected]]` in `$XDG_CONFIG_HOME/slicewise/config.toml`, then run
   `slicewise units <dir>` and start `slicewise.service`.
2. Wherever you call `systemd-run --user …`, call this instead:

   ```
   slicewise run --class interactive|normal|batch [--mem-hint <size>] [--jobs <n>] [--no-queue] -- <cmd>
   ```

3. Pick the class by who waits: a human waiting → `interactive`; can run later → `batch`; unsure → `normal`.
4. Give `--mem-hint` for anything big; that is what lets admission queue a job instead of OOMing two.
5. Builds use the hint: `make -j"$SLICEWISE_JOBS"`.
6. Do not pass `--slice`, `--system`, `-M` or `-H`; `run` refuses them (exit 64).
7. `slicewise status` shows the pool; `slicewise explain <unit>` shows why a unit got its limits.

Example from a shell:

```
slicewise run --class batch --mem-hint 3G -- --wait --pipe sh -c 'make -j"$SLICEWISE_JOBS"'
```

Single quotes matter: the variable is set inside the unit, not in your shell.

No daemon running? `run` still launches, on static defaults, with a warning (§5.7).
