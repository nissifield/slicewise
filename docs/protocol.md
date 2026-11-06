# slicewise socket protocol

Design §6.2 is authoritative for framing, strictness and errors. This file records the payloads as implemented at M1.

## `status` payload

- `cap`, `working_set`, `headroom`, `margin` (bytes).
- `classes[]`: `class`, `memory`, `memory_high`, `cpu_quota`, `cpu_psi`, `memory_psi`, `pids`, `brake_secs`, `clamp_secs`. `memory_high` and `cpu_quota` use `null` for infinity; `cpu_quota` and `clamp_secs` are non-null only for batch.
- `protected[]`: `unit`, `cpu_psi`.
- `queue`.

## `explain` payload

- `target`, `class`, `slice`.
- `events[]`: `age_secs`, `event`, `unit`, `reason` (oldest first).
- `current`: a class row as in `status`.

## Still to extract from design §6

- Socket location, mode and lifetime (`$XDG_RUNTIME_DIR/slicewise.sock`, 0600, no `/tmp` fallback).
- Framing: one request per connection, newline-delimited JSON, 64 KiB limit, one overall 2 s request deadline, 64-connection cap.
- Versioning (`v`) and strictness (`deny_unknown_fields`).
- Requests: `admit`, `status`, `explain`, with every field and its bounds.
- Responses: `admit`, `queued` (and its follow-up line), `reject`, and each error code.
- The `status` and `explain` JSON payloads (design §8 data).
- CLI exit codes (64, 75, 78, 3) and how they map to protocol outcomes.
