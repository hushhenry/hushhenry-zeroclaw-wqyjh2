# Cron & Scheduled Tasks

ZeroClaw provides a robust scheduling system (Cron) that ensures reliability and delivery consistency, even in complex multi-session environments.

## Origin-Locked Delivery

The most critical feature of ZeroClaw's Cron system is **Origin-Locking**. This ensures that the results or reminders of a task are always delivered back to the exact context where the task was created.

- **Session Binding**: When a job is created, the system captures the full routing metadata (Channel, Chat ID, Thread ID, Session ID).
- **Isolation from `/new`**: If a user creates a reminder in a group chat and then runs `/new` to start a fresh session, the Cron task remains bound to the *original* session/thread where it was born.
- **Consistency**:
    - Direct Messages stay in DMs.
    - Group messages return to the same group.
    - Thread-specific tasks stay within that thread.

## Reliability & Stability

- **Timeout Management**: The scheduler includes optimized timeout handling for both task execution and message delivery, preventing "ghost" tasks that fail silently.
- **Legacy Compatibility**: The system maintains backward compatibility for tasks created before the origin-locking feature was introduced, using traditional `delivery.channel/to` routing as a fallback.

## Usage

Tasks are typically managed via the `cron` tool or internal scheduling APIs. The underlying delivery mechanism is handled automatically by the ZeroClaw runtime to maintain the "Origin-Locked" guarantee.
