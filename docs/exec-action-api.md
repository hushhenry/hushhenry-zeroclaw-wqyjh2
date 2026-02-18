# Shell Exec Action API (Background Model)

This document defines the `shell` tool exec-upgrade model.

## Scope

- No new tool name is introduced.
- The existing `shell` tool now supports action multiplexing via `action`.
- Legacy behavior remains backward compatible when `action` is omitted.

## API Shape

`shell` accepts:

- `action`: `spawn` | `poll` | `kill` | `send_keys` (phase-1 stub)
- `command`: required for `action=spawn`
- `background`: optional bool (defaults to `true` for `spawn`)
- `pty`: optional bool (phase-1 stores flag; non-PTY execution)
- `timeout_secs`: optional hard timeout
- `max_output_bytes`: optional output cap
- `watch`: optional watcher list `[{regex,event,once?,scope?}]`
- `run_id`: required for `poll`/`kill`
- `since_seq`: optional incremental cursor for `poll`
- `session_id`: creating session identifier used for routing/backlog

When `action` is omitted, `shell` behaves as the legacy synchronous one-shot command executor.

## Blocking-Avoidance Rule

- `spawn`/`poll`/`kill` return quickly.
- Long-running process execution is handled by a background worker.
- The worker claims queued runs from persisted storage and executes asynchronously.

## Persistence Model

Exec state is persisted in `memory/sessions.db`:

- `exec_runs`: run metadata, status, limits, timestamps, session binding
- `exec_run_items`: streamed output/events with monotonic `seq`

`poll` reads persisted state and returns incremental items using `since_seq`.

## Recovery Model

On worker startup:

- runs left in `running` state are recovered to `queued`
- recovered runs are eligible for normal claim-and-execute processing

This guarantees restart-safe polling and continuation behavior.

## Concurrency Model

- Runs are serialized per `session_id`.
- Runs can execute in parallel across different sessions.

## Watcher Flow to Steer Backlog

1. Worker captures `stdout`/`stderr` chunks while the run is active.
2. Configured regex watchers are evaluated against incoming chunks.
3. On match, worker persists an `event` item in `exec_run_items`.
4. Worker enqueues a backlog message keyed by `session_id`.
5. Existing steer-backlog injection path delivers this message on the next checkpoint turn.

## Wait-on-Conditions Guidance

Use `spawn` + `poll` loops instead of blocking on process completion:

1. `spawn` run and store `run_id`
2. `poll` with `since_seq` for incremental output/events
3. react to watcher events/status transitions
4. call `kill` if a condition indicates cancellation

This keeps the agent loop responsive and avoids long blocking tool calls.
