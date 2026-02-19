# Shell Exec Action API (Persistent Background Runtime)

This document defines the `shell` tool's asynchronous, action-driven execution model.

## Overview

ZeroClaw's shell implementation supports long-running, non-blocking tasks with full lifecycle management. This allows the agent to spawn processes (like build scripts or servers), poll their status, and manage them without blocking the main agent loop.

## API Actions

The `shell` tool supports the following actions:

- **`spawn`**: Asynchronously starts a new process. Requires a `command`.
- **`poll`**: Returns run metadata (status, exit code) and a bounded snippet of recent output.
- **`log`**: Retrieves incremental stdout/stderr logs using a cursor-based approach.
- **`kill`**: Forcefully terminates a running process.

## Key Features

### 1. Non-Blocking & Persistent
- **Background Workers**: Execution is handled by a background worker that claims runs from a persistent queue.
- **Restart-Safe**: Process states are stored in `sessions.db`. If the agent restarts, it can recover and continue tracking \"running\" processes.

### 2. Stream & Watcher Logic
- **Regex Watchers**: You can attach watchers to a process to trigger events based on output patterns.
- **Event Sink**: Watcher hits are decoupled and can be routed back to the session backlog for the agent to process on its next turn.

### 3. Output Management
- **Snippets**: `poll` returns a small \"tail\" of the output for quick inspection.
- **Cursors**: `log` uses `since_seq` and `next_seq` to allow paging through massive log files without loading everything into memory.

## Usage Guidance

Always prefer `spawn` + `poll` for any command expected to take more than a few seconds. This keeps the agent responsive and allows it to perform other tasks while waiting for results.
