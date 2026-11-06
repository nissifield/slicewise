# slicewise socket protocol

Extracted from design.md §6 at M1; until then design.md is authoritative.

Will contain:

- Socket location, mode and lifetime (`$XDG_RUNTIME_DIR/slicewise.sock`, 0600, no `/tmp` fallback).
- Framing: one request per connection, newline-delimited JSON, 64 KiB limit, 2 s read timeout, 64-connection cap.
- Versioning (`v`) and strictness (`deny_unknown_fields`).
- Requests: `admit`, `status`, `explain`, with every field and its bounds.
- Responses: `admit`, `queued` (and its follow-up line), `reject`, and each error code.
- The `status` and `explain` JSON payloads (design §8 data).
- CLI exit codes (64, 75, 78, 3) and how they map to protocol outcomes.
