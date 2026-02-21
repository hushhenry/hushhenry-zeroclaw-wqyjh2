# Session 管理

ZeroClaw 的 session 架构为每次对话提供持久化会话标识与上下文隔离。

- **Session 解析**：`SessionResolver` 将 channel 元数据规范为 `SessionKey`（thread > group > DM）。
- **存储**：SQLite `workspace/memory/sessions.db`（session_index、sessions、session_messages、session_state 等）。
- **Session 模式**：`config.session.enabled = true` 时，agent 以 session 为单位运行；上下文从 DB 加载一次后只写不读，压缩基于内存 history。

**持久化与压缩的详细约定**（入库时机、上下文重建、尾部孤立 user、in-memory 压缩）见 [Session 持久化与压缩](session-persistence-and-compaction.md)。

## 配置

```toml
[session]
enabled = true
history_limit = 40
```
