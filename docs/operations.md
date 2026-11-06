# slicewise operations

Extracted from design.md §5.7 / §11 / §12 at M1; until then design.md is authoritative.

Will contain:

- Preconditions: cgroup v2, a systemd user manager with `cpu` and `memory` delegated, and how to check.
- Install without root: `cargo install --path . --locked --offline`, `slicewise units <dir>`, enabling
  `slicewise.service`.
- Configuration walkthrough and `slicewise check-config`.
- Migration from a runner's plain `systemd-run` prefix to `slicewise run` (M2).
- Day-to-day: `status`, `explain`, reading the logfmt lines in the user journal.
- Fail-safe behaviour and what an operator sees in each case.
- Rollback: `slicewise reset`, stop the service, restore the old runner argv, remove the unit files.
- Live verification (`SLICEWISE_LIVE=1 cargo test -- --ignored`) and when it was last rehearsed.
