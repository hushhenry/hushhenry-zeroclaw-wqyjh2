//! Memory tool: action-based API (store, recall, forget) for long-term memory.
//!
//! - **store**: store a fact, preference, or note (key, content, optional category).
//! - **recall**: search memory by query; returns scored results.
//! - **forget**: remove a memory by key.

use super::traits::{Tool, ToolResult};
use crate::memory::{Memory, MemoryCategory};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;

const ACTION_STORE: &str = "store";
const ACTION_RECALL: &str = "recall";
const ACTION_FORGET: &str = "forget";

pub struct MemoryTool {
    memory: Arc<dyn Memory>,
}

impl MemoryTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Long-term memory: store, recall, or forget (action: store | recall | forget)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [ACTION_STORE, ACTION_RECALL, ACTION_FORGET],
                    "description": "Action: store (save), recall (search), forget (delete by key)"
                },
                "key": {
                    "type": "string",
                    "description": "Unique key (store/forget); e.g. 'user_lang', 'project_stack'"
                },
                "content": {
                    "type": "string",
                    "description": "Information to remember (store)"
                },
                "category": {
                    "type": "string",
                    "enum": ["core", "daily", "conversation"],
                    "description": "Memory category: core (permanent), daily (session), conversation (chat)"
                },
                "query": {
                    "type": "string",
                    "description": "Search keywords or phrase (recall)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results for recall (default: 5)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'action' parameter (store | recall | forget)".to_string()),
                });
            }
        };

        match action {
            ACTION_STORE => self.handle_store(args).await,
            ACTION_RECALL => self.handle_recall(args).await,
            ACTION_FORGET => self.handle_forget(args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid action '{action}'; expected store, recall, or forget"
                )),
            }),
        }
    }
}

impl MemoryTool {
    async fn handle_store(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = match args
            .get("key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(k) => k,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'key' for store action".to_string()),
                });
            }
        };

        let content = match args
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'content' for store action".to_string()),
                });
            }
        };

        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("daily") => MemoryCategory::Daily,
            Some("conversation") => MemoryCategory::Conversation,
            _ => MemoryCategory::Core,
        };

        match self.memory.store(key, content, category, None).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Stored memory: {key}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to store memory: {e}")),
            }),
        }
    }

    async fn handle_recall(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = match args
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(q) => q,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'query' for recall action".to_string()),
                });
            }
        };

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        match self.memory.recall(query, limit, None).await {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No memories found matching that query.".into(),
                error: None,
            }),
            Ok(entries) => {
                let mut output = format!("Found {} memories:\n", entries.len());
                for entry in &entries {
                    let score = entry
                        .score
                        .map_or_else(String::new, |s| format!(" [{s:.0}%]"));
                    let _ = writeln!(
                        output,
                        "- [{}] {}: {}{score}",
                        entry.category, entry.key, entry.content
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Memory recall failed: {e}")),
            }),
        }
    }

    async fn handle_forget(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = match args
            .get("key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(k) => k,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'key' for forget action".to_string()),
                });
            }
        };

        match self.memory.forget(key).await {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Forgot memory: {key}"),
                error: None,
            }),
            Ok(false) => Ok(ToolResult {
                success: true,
                output: format!("No memory found with key: {key}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to forget memory: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, SqliteMemory};
    use tempfile::TempDir;

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[test]
    fn memory_tool_name_and_schema() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        assert_eq!(tool.name(), "memory");
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(schema["properties"]["key"].is_object());
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("action")));
    }

    #[tokio::test]
    async fn memory_missing_action_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool.execute(json!({})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("action"));
    }

    #[tokio::test]
    async fn memory_invalid_action_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool.execute(json!({ "action": "invalid" })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("Invalid action"));
    }

    #[tokio::test]
    async fn memory_store_and_recall() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem.clone());
        let r = tool
            .execute(json!({"action": "store", "key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("Stored memory: lang"));

        let r = tool
            .execute(json!({"action": "recall", "query": "Rust"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("Rust"));
    }

    #[tokio::test]
    async fn memory_store_with_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem.clone());
        let r = tool
            .execute(json!({
                "action": "store",
                "key": "note",
                "content": "Session note",
                "category": "daily"
            }))
            .await
            .unwrap();
        assert!(r.success);
        let entry = mem.get("note").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().category, MemoryCategory::Daily);
    }

    #[tokio::test]
    async fn memory_recall_empty_returns_message() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool
            .execute(json!({"action": "recall", "query": "nonexistent"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn memory_recall_missing_query_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool.execute(json!({ "action": "recall" })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("query"));
    }

    #[tokio::test]
    async fn memory_recall_respects_limit() {
        let (_tmp, mem) = test_mem();
        for i in 0..10 {
            mem.store(
                &format!("k{i}"),
                &format!("Rust fact {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }
        let tool = MemoryTool::new(mem);
        let r = tool
            .execute(json!({"action": "recall", "query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn memory_forget_existing() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let tool = MemoryTool::new(mem.clone());
        let r = tool
            .execute(json!({"action": "forget", "key": "temp"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("Forgot"));
        assert!(mem.get("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_forget_nonexistent_returns_success_no_found() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool
            .execute(json!({"action": "forget", "key": "nope"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("No memory found"));
    }

    #[tokio::test]
    async fn memory_forget_missing_key_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool.execute(json!({ "action": "forget" })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("key"));
    }

    #[tokio::test]
    async fn memory_store_missing_key_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool
            .execute(json!({"action": "store", "content": "no key"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("key"));
    }

    #[tokio::test]
    async fn memory_store_missing_content_returns_error() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryTool::new(mem);
        let r = tool
            .execute(json!({"action": "store", "key": "x"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("content"));
    }
}
