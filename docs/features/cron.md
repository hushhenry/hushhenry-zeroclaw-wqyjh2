# Cron & Scheduled Task Delivery

The ZeroClaw Cron system provides a durable mechanism for scheduling one-shot reminders or recurring tasks. Its primary goal is ensuring that scheduled outputs are delivered back to the user in a consistent and expected manner, regardless of changes in the agent's active session.

## 1. Origin-Locked Routing (Session-Bound Delivery)

The most significant challenge in multi-session environments is "Routing Drift"—where a reminder created in one group chat accidentally appears in another, or a thread-specific task gets lost in the main group chat.

### How Origin-Locking Works
When a task is created (`cron add` or via tool call), ZeroClaw "stamps" the job with the full routing metadata of the creator:
- **`channel_id`**: The platform (e.g., Feishu, Telegram).
- **`chat_id`**: The specific group or user ID.
- **`thread_id`**: The specific thread or topic within a group (if applicable).
- **`session_id`**: The unique session active at the moment of creation.

### Guaranteed Delivery Path
When the cron worker triggers:
1. It retrieves the stored routing metadata from the database.
2. It bypasses the *current* active session logic.
3. It delivers the payload directly to the **original** source.

**Example Scenario**:
- A user starts a task in **Group A, Thread 5**.
- The user then switches context and runs `/new` in the main **Group A** chat.
- When the task completes, ZeroClaw delivers the result specifically to **Group A, Thread 5**, ensuring it doesn't interrupt the new conversation in the main chat.

## 2. Legacy Compatibility

To ensure a smooth transition from older versions of ZeroClaw, the system implements a fallback mechanism:
- If a job lacks origin-locking metadata (e.g., it was created before this feature was implemented), the system falls back to the `delivery.channel` and `delivery.to` fields.
- This ensures that existing scheduled tasks are not broken during the upgrade.

## 3. Reliability & Timeout Stability

Cron tasks often involve external operations or long-running sub-agents. The system includes several stability improvements:
- **Execution Guardrails**: Improved timeout logic prevents a single hanging task from blocking the cron worker queue.
- **Delivery Retries**: Enhancements in the channel delivery layer ensure that transient network issues do not cause missed notifications.

## 4. Usage in Tools

When using the `cron` tool, the agent automatically populates the routing fields.
- **`sessionTarget`**: Can be set to `"main"` (inject as a system event in the original session) or `"isolated"` (run as a standalone agent turn).
- **`payload`**: Defines what actually happens (text notification or agent command).

```json
{
  "action": "add",
  "job": {
    "name": "Meeting Reminder",
    "schedule": { "kind": "at", "at": "2026-02-19T15:00:00Z" },
    "payload": { "kind": "systemEvent", "text": "Time for the sync!" },
    "sessionTarget": "main"
  }
}
```
