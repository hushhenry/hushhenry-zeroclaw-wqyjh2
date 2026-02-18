# Shell Exec Action API (Background Model)

This document defines the `shell` tool exec-upgrade model.

## Scope

- No new tool name is introduced.
- The existing `shell` tool now supports action multiplexing via `action`.
- Legacy behavior remains backward compatible when `action` is omitted.

## API Shape

`shell` accepts:

- `action`: `spawn` | `poll` | `log` | `kill` | `send_keys` (phase-1 stub)
- `command`: required for `action=spawn`
- `background`: optional bool (defaults to `true` for `spawn`)
- `pty`: optional bool (phase-1 stores flag; non-PTY execution)
- `timeout_secs`: optional hard timeout
- `max_output_bytes`: optional output cap
- `watch`: optional watcher list `[{regex,event,once?,scope?}]`
- `run_id`: required for `poll`/`log`/`kill`
- `since_seq`: optional incremental cursor for `poll` and `log`
- `limit`: optional max items for `action=log` (server-capped)
- `stream`: optional `"stdout"` or `"stderr"` filter for `action=log`
- `session_id`: creating session identifier used for routing/backlog

When `action` is omitted, `shell` behaves as the legacy synchronous one-shot command executor.

## Poll vs Log Semantics

- **`poll`** is for state checks: it returns run metadata (`status`, `exit_code`, `truncated`, `error_message`, timestamps), a bounded **snippet** of recent output (small, stable size), and incremental `items` with `next_seq`. Use it to check run status and get a quick tail of output without fetching full history.
- **`log`** is for ranged output/event retrieval: it reads `exec_run_items` strictly after `since_seq` and up to `limit` (server-capped), and returns `items` and `next_seq`. Optional `stream` filters to `stdout` or `stderr` only. Use it to page through full output or events when you need more than the snippet (e.g. after completion, or for logs/audit).
- Both use the same cursor contract: pass `since_seq` from the previous response’s `next_seq` to get the next batch.

## Blocking-Avoidance Rule

- `spawn`/`poll`/`kill` return quickly.
- Long-running process execution is handled by a background worker.
- The worker claims queued runs from persisted storage and executes asynchronously.

## Persistence Model

Exec state is persisted in `memory/sessions.db`:

- `exec_runs`: run metadata, status, limits, timestamps, session binding
- `exec_run_items`: streamed output/events with monotonic `seq`

`poll` and `log` both read from `exec_run_items`; `poll` also returns run row and a bounded snippet. Cursor `since_seq` / `next_seq` is shared.

## Retention Notes

`exec_run_items` grows with command output and watcher events. Keep retention operationally bounded by periodic DB maintenance/compaction (e.g. pruning old completed runs and their items) so `sessions.db` growth remains predictable. The same cursor-based `log` API works regardless of retention policy.

## Recovery Model

On worker startup:

- runs left in `running` state are recovered to `queued`
- recovered runs are eligible for normal claim-and-execute processing

This guarantees restart-safe polling and continuation behavior.

## Concurrency Model

- Runs are serialized per `session_id`.
- Runs can execute in parallel across different sessions.

## Snippet and Callback Guidance

- **Snippet**: Poll responses and runtime event callbacks include a bounded recent-output snippet (same builder: tail of concatenated stdout/stderr, capped in size). This lets consumers react without issuing an immediate extra `poll`.
- **Event sink**: Notifications are decoupled from backlog via an `ExecEventSink` trait (status change, watcher hit, output). The default sink bridges watcher hits to the existing session backlog enqueue behavior. Custom sinks can implement the same trait and receive the same bounded snippet in callbacks.
- **Output caps**: Per-chunk and total caps avoid huge stdout/stderr amplification (see `shell_exec_runtime` module docs and constants: `CHUNK_MAX_CHARS`, `SNIPPET_MAX_CHARS`, run `max_output_bytes`).

## Watcher Flow to Steer Backlog

1. Worker captures `stdout`/`stderr` chunks while the run is active (chunks and callback payloads are capped).
2. Configured regex watchers are evaluated against incoming chunks.
3. On match, worker persists an `event` item in `exec_run_items`.
4. Worker invokes the event sink (default: enqueues a backlog message keyed by `session_id`, with a bounded snippet available to the callback).
5. Existing steer-backlog injection path delivers this message on the next checkpoint turn.

## Wait-on-Conditions Guidance

Use `spawn` + `poll` (or `log`) instead of blocking on process completion:

1. `spawn` run and store `run_id`
2. `poll` with `since_seq` for status and incremental output/events (and a bounded snippet), or use `log` with `since_seq`/`limit`/`stream` to page through full output
3. React to watcher events/status transitions
4. Call `kill` if a condition indicates cancellation

This keeps the agent loop responsive and avoids long blocking tool calls.
