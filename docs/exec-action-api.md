# Shell Exec Action API (Persistent Background Runtime)

This document defines the architecture and API for the `shell` tool's persistent, asynchronous execution model.

## 1. Overview & Motivation

Traditional `shell` executions are blocking—the agent waits for the command to finish before moving to the next turn. This is impractical for long-running tasks like compiling large projects, running servers, or monitoring logs.

The **Exec Action API** introduces a non-blocking, action-driven model that allows the agent to:
1. **Spawn** a process and immediately regain control.
2. **Poll** for status or snippets of output.
3. **Log** through large outputs using incremental cursors.
4. **Kill** processes that are no longer needed.

## 2. API Schema

The `shell` tool accepts an `action` parameter. If omitted, it defaults to legacy synchronous execution.

### Actions
- **`spawn`**: Starts a process in the background.
    - `command`: The shell command to run.
    - `background`: Defaults to `true`.
    - `watch`: Optional list of regex patterns to monitor (see **Watchers** below).
- **`poll`**: Quick status check.
    - `run_id`: The identifier returned by `spawn`.
    - Returns: Status (`running`, `success`, `failed`), exit code, and a **small output snippet** (tail).
- **`log`**: Incremental log retrieval.
    - `run_id`: The identifier.
    - `since_seq`: Monotonic sequence number to fetch logs *after* this point.
    - `limit`: Max items to return.
- **`kill`**: Terminates a running process.

## 3. Persistent Execution Model

Process states and outputs are persisted in the `sessions.db` database.

### Database Tables
- **`exec_runs`**: Stores metadata, run status, process IDs (PID), and the session the task belongs to.
- **`exec_run_items`**: Stores every chunk of `stdout` and `stderr`, along with events (like watcher hits), each assigned a monotonic `seq` ID.

### Restart Recovery
ZeroClaw is "Restart-Safe." If the agent process is killed or restarted:
1. Upon startup, the background worker scans `exec_runs` for any tasks still marked as `running`.
2. These tasks are marked as `queued` or `recovered`.
3. The worker attempts to resume tracking or cleanly handles the orphaned process state.

## 4. Watcher Flow & Backlog Steering

Watchers allow the agent to "subscribe" to specific output patterns without constantly polling.

1. **Detection**: As the process runs, the worker matches chunks of output against the `regex` patterns defined during `spawn`.
2. **Persistence**: Every match is recorded as an `event` in `exec_run_items`.
3. **Decoupled Sink**: The `ExecEventSink` trait allows the worker to notify the agent. The default implementation enqueues a message into the **Session Backlog**.
4. **Steering**: On the agent's next turn, it sees the backlog message (e.g., `"Build finished successfully"`) and can react accordingly.

## 5. Output Management & Snippets

To prevent large logs from overwhelming the agent's context:
- **Snippets**: Every `poll` and watcher notification includes a bounded "snippet" (the last ~1-2 KB of output). This gives the agent enough context to decide if it needs to fetch full logs.
- **Cursors**: Using `since_seq` and `next_seq` in the `log` action allows the agent to page through megabytes of logs incrementally, rather than loading everything at once.

## 6. Wait-on-Conditions Guidance

Instead of blocking, the agent should follow this pattern:
1. Call `spawn` with a `watch` pattern for the expected success/error message.
2. Inform the user that the task is running in the background.
3. Wait for the watcher event to appear in the backlog or periodically `poll` if no specific pattern is known.
4. Use `log` to fetch details only if an error is detected.
