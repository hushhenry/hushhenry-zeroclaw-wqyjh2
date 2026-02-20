# Cron 结果/提醒推送需求检查

## 需求摘要

1. **指定目标会话**：创建 cron 任务时可以指定「结果和提醒推送到哪个会话」。
2. **默认目标**：不指定时，默认使用「创建该任务的会话」。
3. **推送方式**：通过 **internal channel** 推送提醒。

---

## 当前实现概览

### 1. 创建时记录的会话信息

| 机制 | 说明 |
|------|------|
| `source_session_id` | 创建者会话 ID。由 agent loop 在调用 `cron_add` / `cron_update` / `schedule` 时自动注入（见 `src/agent/loop_.rs`），无需用户传。 |
| 工具参数 | `cron_add` 支持可选 `source_session_id`；`schedule` 支持可选 `source_session_id`，用于兼容/覆盖。 |

### 2. 结果/提醒投递（delivery）逻辑

- **触发条件**：`job.delivery.mode == "announce"` 时才会执行结果/提醒推送（`cron/scheduler.rs` 中 `deliver_if_configured`）。
- **路由解析**（`resolve_delivery_route`）：
  1. **优先**用 `job.source_session_id` 查 session store 的 route metadata → 得到 `channel` + `target`（如 telegram chat_id）。
  2. 若没有 `source_session_id` 或查不到 metadata，则用 `delivery.channel` + `delivery.to`。
- **实际发送**：`send_via_channel(config, channel, target, output)`，仅支持**外部 channel**（telegram / discord / slack / mattermost / lark 等），见 `scheduler.rs` 中 `send_via_channel` 分支。

### 3. Agent 任务执行与 internal channel

- **执行请求**：agent 类 cron 通过 `build_internal_channel_message` + `dispatch_internal_message` 把**执行请求**发到 internal channel（main 或 isolated session），这部分已走 internal。
- **结果/提醒**：执行完成后的**结果与提醒**在 `persist_job_result` → `deliver_if_configured` 中只走 `send_via_channel`，即**仅外部 channel**，未走 internal channel。

---

## 需求符合性

| 需求 | 状态 | 说明 |
|------|------|------|
| 不指定则默认使用「创建它的那个会话」 | ✅ 已满足 | `source_session_id` 自动注入；`resolve_delivery_route` 优先用其查 route，等价于「默认推送到创建者会话」（在开启 announce 且能解析到 route 时）。 |
| 创建时可**指定**结果和提醒推送到哪个会话 | ⚠️ 部分满足 | 无「目标 session_id」一等概念。只能通过 `delivery.channel` + `delivery.to` 指定具体外部 channel 的 target（如 telegram chat_id），不能直接指定「推送到某会话」。 |
| 通过 **internal channel** 推送提醒 | ❌ 未满足 | 当前仅支持经外部 channel 的 announce；没有「经 internal channel 发到某 session_id」的 delivery 路径。 |

---

## 结论与建议

- **默认使用创建者会话**：逻辑已有，依赖 `source_session_id` 与 session route metadata。
- **指定推送到某会话**：缺少「目标会话」抽象，建议增加 `delivery_session_id`（或等价字段），不指定时回退为 `source_session_id`。
- **通过 internal channel 推送**：需新增 delivery 路径，例如：
  - 新增 `delivery.mode = "internal"`（或约定当 `delivery.channel == "internal"` 时），目标由 `delivery_session_id` 或 `source_session_id` 决定；
  - 在 `deliver_if_configured` 中对该模式调用 `build_internal_channel_message(target_session_id, content)` + `dispatch_internal_message(msg)`，由现有 channel 派发逻辑把消息投递到对应会话。

若希望「提醒一律走 internal channel」，可进一步约定：在 daemon 下默认使用 internal 投递到目标 session，仅在没有 internal 派发器或显式配置时才回退到外部 channel。
