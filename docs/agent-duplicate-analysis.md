# Agent 模块重复逻辑分析

本文档分析 `src/agent/` 及相关路径（channels、session）中潜在的重复逻辑与未使用代码，便于后续按 CLAUDE/AGENTS 原则做收敛或清理。

---

## 1. 执行路径结论（上下文）

- **Agent 的 run/turn 已删除**。当前 `Agent` 只有 `builder()`、`history()`、`clear_history()`、`from_config()`，**没有** `run()` / `turn()` 等执行入口；执行逻辑全部在 `turn.rs` 与 `loop_.rs`。
- **当前唯一生产执行路径**：Channel 消息 → `turn::run_turn_core` → `loop_::run_tool_call_loop`。使用 `ChannelRuntimeContext`、`channels::build_system_prompt`、`loop_.rs` 内联的 tool-call 解析与工具执行，**完全不经过** `Agent` 结构体。
- **Agent 结构体现状**：通过 `Agent::from_config` 仍会构建并持有 `ToolDispatcher`、`SystemPromptBuilder`、`history` 等（`memory_loader` 已删除），但因 **没有 run/turn 入口**，这些字段在现有主流程中 **不会被使用**（仅测试里会构建 Agent，且测试里实际执行走的是 `loop_::run_one_turn_for_test` → `run_tool_call_loop`）。

因此：**不存在“两套执行路径”**，只有一套（turn + loop_）；Agent 更像是历史遗留的“配置/组件持有者”，其内部的 dispatcher / prompt_builder / memory_loader 与当前执行路径脱节。

---

## 2. 重复逻辑与问题点

### 2.1 Tool-call 解析（高重复）

| 位置 | 能力 | 实际使用方 |
|------|------|------------|
| `agent/loop_.rs` | `parse_tool_calls`、`parse_tool_calls_from_json_value`、`parse_structured_tool_calls`；支持 `<tool_call>` / `<toolcall>` / `<tool-call>`、OpenAI 风格 JSON、安全约束（仅解析带标签或 tool_calls 数组的内容） | **生产路径**：`run_tool_call_loop` |
| `agent/dispatcher.rs` | `XmlToolDispatcher::parse_xml_tool_calls` 仅支持 `<tool_call>`/`</tool_call>`；`NativeToolDispatcher::parse_response` 从 `ChatResponse.tool_calls` 取原生格式 | 仅 **agent/tests.rs** 中 dispatcher 相关单测 |

**问题**：两处都在做“从模型输出中解析 tool call”，语义重叠。loop_.rs 实现更完整（多标签、OpenAI 格式、安全策略），且是唯一被生产使用的；dispatcher 的解析与 loop_.rs 不一致且能力更窄。Agent 已无 run/turn，dispatcher 仅在测试里被直接用来测 `parse_response`，易造成“测试通过、生产行为不同”的认知偏差。

**建议**（任选其一或分阶段）：  
- 让 `ToolDispatcher::parse_response` 在需要时委托给 loop_.rs 的解析逻辑，或抽成共享解析模块供两处使用，保证单一路径、一种行为。  
- 或将 dispatcher 的解析收敛为对 loop_.rs 的薄封装，并逐步统一测试。

---

### 2.2 Memory context 构建（已收敛）

**已实施**：`agent/loop_.rs::build_context` 已删除（由 `channels::build_memory_context` 替代）；`agent/memory_loader.rs` 模块已删除（无调用方）。当前仅 `channels/mod.rs::build_memory_context` 负责“按 query 拉取 memory 并格式化为 [Memory context] 字符串”，供 `turn::run_turn_core` 非 session 路径使用。

---

### 2.3 System prompt 构建（两套实现）

| 位置 | 形态 | 使用方 |
|------|------|--------|
| `agent/prompt.rs` | `SystemPromptBuilder` + 多个 `PromptSection`（Identity, Tools, Safety, Skills, Workspace, DateTime, Runtime） | Agent 构建时写入，但 Agent 无 run/turn，故未接入任何执行路径 |
| `channels/mod.rs` | `build_system_prompt(workspace_dir, model_name, tools, skills, identity_config, bootstrap_max_chars)`，大函数内联实现类似区块（Tools、Tool Use Protocol、Safety、Skills、Workspace、Project Context 等） | **生产路径**：`turn::run_turn_core` 中构建 system prompt |

**问题**：同一产品的“系统提示”有两套独立实现，结构和内容高度相似（工具、安全、技能、工作区、身份/项目上下文等），但细节和可配置性不同。因 Agent 已无执行入口，prompt.rs 那套目前未参与执行，长期会导致行为分叉和重复修 bug（若将来复用 Agent 组件）。

**建议**：  
- **与 main 对齐**：zeroclaw main 分支（<https://github.com/zeroclaw-labs/zeroclaw/tree/main>）上 `agent/prompt.rs` 的 `SystemPromptBuilder` 已具备 `PromptContext.skills_prompt_mode`、`SkillsSection` 使用 `skills_to_prompt_with_mode` 等能力；channels 侧若仍用大函数 `build_system_prompt`，后续应以 main 为参考，逐步改为用 `SystemPromptBuilder` 生成 base prompt，channels 仅做 `build_merged_system_prompt`（或等价）叠加 delivery 说明，实现“SystemPromptBuilder 替代 build_system_prompt”。  
- 若短期不统一，至少在文档中标明“当前仅 channels::build_system_prompt 参与执行；Agent 的 SystemPromptBuilder 无调用方”，并列出差异，避免误改。

---

### 2.4 工具列表过滤（已统一）

**已实施**：统一使用 `loop_::tools_to_specs`。`turn.rs` 中构建 `tool_entries` 时改为调用 `tools_to_specs(ctx.tools_registry.as_ref(), allowed_tools.as_deref())`，再从返回的 `ToolSpec` 列表得到 `(name, description)` 供 `build_system_prompt` 使用，与 `run_tool_call_loop` 的过滤策略一致。

---

### 2.5 历史压缩（已收敛为存储侧）

**已实施**：压缩与存储结合，不再保留 in-memory 压缩。已从 `agent/loop_.rs` 删除：`trim_history`、`auto_compact_history`、`build_compaction_transcript`、`apply_compaction_summary` 及相关常量和单测。当前仅 `session/compaction.rs` 的持久化压缩（`maybe_compact` 等）在 `turn::run_turn_core` 的 `use_session_history` 路径下使用。

---

## 3. 小结表

| 类别 | 状态 | 说明 |
|------|------|------|
| Tool-call 解析 | 待收敛 | loop_.rs 与 dispatcher 两套；仅 loop_.rs 用于生产，建议后续统一 |
| Memory context | **已收敛** | 已删 build_context 与 memory_loader，仅 channels::build_memory_context |
| System prompt | 待对齐 main | 以 main 为参考用 SystemPromptBuilder 替代 build_system_prompt，见 §2.3 |
| 工具 allow-list | **已统一** | turn.rs 使用 loop_::tools_to_specs |
| 历史压缩 | **已收敛** | 已删 in-memory 压缩，仅保留 session/compaction 存储侧 |

---

## 4. 与协议对齐

- **KISS/DRY**：上述重复均适合在“明确单一调用路径”后做收敛，避免同一语义多处实现。  
- **YAGNI**：loop_.rs 中未使用的 `build_context`、未接入的 `trim_history`/`auto_compact_history` 等，可按 YAGNI 删除或收窄到测试专用。  
- **架构边界**：收敛时保持“orchestration 在 agent/、transport 在 channels/、session 状态在 session/”，避免把 channel 或 session 细节塞进 loop_.rs 的 core loop；共享逻辑放在 agent 或公共 util，由 turn 与 loop_ 调用。

以上为对 agent 及相关路径的重复逻辑分析，便于后续排期做去重与路径统一。
