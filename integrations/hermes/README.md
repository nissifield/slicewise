# slicewise for Hermes Agent

**Status: M2 work (design §12). Nothing here exists yet.** This page describes how Hermes Agent, or any task
runner built on it, will adopt slicewise once M1 ships. Paths are placeholders: `$HERMES_HOME` is the Hermes
data directory, `<runner>` is whatever module launches delegated tasks.

## 1. Protect the gateway

The Hermes gateway is the orchestrator, so it goes in the **protected** class (design §5.2). It is not launched
through `slicewise run`; it is named in `$XDG_CONFIG_HOME/slicewise/config.toml`:

```toml
[[protected]]
unit       = "<hermes-gateway>.service"
cpu_weight = 200
memory_low = "2G"   # size for your host
```

`slicewise units <dir>` writes the matching drop-in. slicewise never writes to this unit at runtime (§3).

## 2. Change the runner's argv prefix

Where `<runner>` builds a `systemd-run` command, replace the prefix (design §6.3):

```
before: systemd-run --user --slice=<tasks>.slice -p CPUQuota=… -p MemoryHigh=… <cmd>
after:  slicewise run --class <class> [--mem-hint <size>] [--jobs <n>] -- <cmd>
```

- Drop the caller's `--slice`, `CPUQuota` and `MemoryHigh`: `run` refuses `--slice` (exit 64), and limits now come
  from the class.
- Omit `--jobs` to let slicewise compute the hint; builds read `$SLICEWISE_JOBS` (`make -j"$SLICEWISE_JOBS"`).
- Exit code 75 means "would queue" and appears only with `--no-queue`. Without it, batch tasks wait for room.
- If the daemon is down, `run` launches anyway on static defaults and warns on stderr (§5.7).

## 3. Task type → class (owner decision, design §14)

| Hermes task | Class |
|---|---|
| Background delegated agent tasks (coding and review runs) and builds | `batch` |
| Chat replies | `interactive` |
| Scheduled (cron) jobs | `interactive` |
| The gateway itself | protected (config, step 1) |
| Everything else | `normal` |

## 4. Background commands outside the runner (M2)

Scopes the gateway starts itself can be classed with an `[[adopt]]` pattern (design §6.3, §7):

```toml
[[adopt]]
match = "<hermes>-worker-*.scope"
class = "normal"
```

## Rollback

Restore the runner's previous `systemd-run` prefix, then follow the rollback in `docs/operations.md` (M2).
