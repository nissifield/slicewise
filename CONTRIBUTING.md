# Contributing

- **Design first.** `docs/design.md` is the source of truth; propose design changes before code.
- **Milestone order.** M0 → M1 → M2 (design §12). Nothing from a later milestone lands early.
- **Gate** before a PR, with each result in the description:
  `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` · `cargo test`
  (plus `SLICEWISE_LIVE=1 cargo test -- --ignored` for changes to `daemon.rs`, `systemd.rs` or `policy.rs`).
- **Dependencies** need approval and an updated design §11 table; `Cargo.lock` changes go in their own PR.
- **No host-specific facts** (hostnames, usernames, home paths, IPs, real machine sizes) in any file.
- **DCO / sign-off is not required.**
- **Licence:** contributions are licensed Apache-2.0 OR MIT, at the user's option, with no extra terms.
- Humans and agents alike: read [`AGENTS.md`](AGENTS.md).
