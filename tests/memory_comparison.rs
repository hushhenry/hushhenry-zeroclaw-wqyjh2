//! Memory backend comparison tests.
//!
//! Milestone 0 decision: Zeroclaw is **sqlite-only** for memory.
//! The previous head-to-head SQLite vs Markdown comparison has been removed to
//! reduce complexity and avoid maintaining multiple backends.

use tempfile::TempDir;
use zeroclaw::memory::{Memory, MemoryCategory, SqliteMemory};

#[tokio::test]
async fn sqlite_memory_smoke_test() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).expect("SQLite init failed");

    mem.store("k1", "hello", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem.recall("hello", 10, None).await.unwrap();
    assert!(!results.is_empty());
}
