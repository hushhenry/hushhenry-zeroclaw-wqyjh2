# Steer-Backlog & Concurrency

ZeroClaw implements a non-blocking interaction model called **Steer-Backlog** to handle concurrent messages and background events in a single-agent conversation.

## 1. The Busy Session Problem
In a standard agent loop, if a user sends a message while the agent is still executing tools for a previous message, the new message might be lost or cause race conditions. 

ZeroClaw solves this using a **Session Turn Guard**. Each session can only have one active agent turn at a time.

## 2. Steer-Backlog Mode
When a message arrives for a "busy" session:
1.  **Detection**: The `SessionTurnGuard` fails to acquire the lock.
2.  **Backlog Enqueue**: The message is placed into an in-memory **Backlog Queue** for that session.
3.  **Notification**: The user is notified: `⏳ Session is busy. Your message was queued with mode steer-backlog and will be applied as steering context shortly.`

## 3. Steering Injection
When the *current* turn finishes and the agent begins its *next* turn:
1.  **Drain**: The system drains all messages from the session's backlog.
2.  **Injection**: These messages are bundled into a single user message prefixed with `[Backlog]`.
3.  **Steering**: The agent receives this backlog context at the start of its new turn, allowing it to "steer" its behavior based on the user's latest updates or corrections.

## 4. Background Event Steering
Steer-Backlog isn't just for user messages. It's the primary channel for background events:
- **Shell Watchers**: When a background process hits a regex match, the event is enqueued in the backlog.
- **Cron Results**: Asynchronous cron tasks can inject their output into the backlog of the original session.
- **Sub-Agent Pings**: When a sub-agent completes a task, it can notify the parent via the backlog.

This decoupling ensures the agent remains responsive and "aware" of background progress without requiring constant polling.
