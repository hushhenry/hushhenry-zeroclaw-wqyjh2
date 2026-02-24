use super::types::{BudgetCheck, CostRecord, CostSummary, ModelStats, TokenUsage, UsagePeriod};
use crate::config::schema::CostConfig;
use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::params;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Cost tracker for API usage monitoring and budget enforcement.
pub struct CostTracker {
    config: CostConfig,
    storage: Arc<Mutex<CostStorage>>,
    session_id: String,
    session_costs: Arc<Mutex<Vec<CostRecord>>>,
}

impl CostTracker {
    /// Create a new cost tracker.
    pub fn new(config: CostConfig, workspace_dir: &Path) -> Result<Self> {
        let storage_path = resolve_storage_path(workspace_dir)?;

        let storage = CostStorage::new(&storage_path).with_context(|| {
            format!("Failed to open cost storage at {}", storage_path.display())
        })?;

        Ok(Self {
            config,
            storage: Arc::new(Mutex::new(storage)),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_costs: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lock_storage(&self) -> MutexGuard<'_, CostStorage> {
        self.storage.lock()
    }

    fn lock_session_costs(&self) -> MutexGuard<'_, Vec<CostRecord>> {
        self.session_costs.lock()
    }

    /// Check if a request is within budget.
    pub fn check_budget(&self, estimated_cost_usd: f64) -> Result<BudgetCheck> {
        if !self.config.enabled {
            return Ok(BudgetCheck::Allowed);
        }

        if !estimated_cost_usd.is_finite() || estimated_cost_usd < 0.0 {
            return Err(anyhow!(
                "Estimated cost must be a finite, non-negative value"
            ));
        }

        let mut storage = self.lock_storage();
        let (daily_cost, monthly_cost) = storage.get_aggregated_costs()?;

        // Check daily limit
        let projected_daily = daily_cost + estimated_cost_usd;
        if projected_daily > self.config.daily_limit_usd {
            return Ok(BudgetCheck::Exceeded {
                current_usd: daily_cost,
                limit_usd: self.config.daily_limit_usd,
                period: UsagePeriod::Day,
            });
        }

        // Check monthly limit
        let projected_monthly = monthly_cost + estimated_cost_usd;
        if projected_monthly > self.config.monthly_limit_usd {
            return Ok(BudgetCheck::Exceeded {
                current_usd: monthly_cost,
                limit_usd: self.config.monthly_limit_usd,
                period: UsagePeriod::Month,
            });
        }

        // Check warning thresholds
        let warn_threshold = f64::from(self.config.warn_at_percent.min(100)) / 100.0;
        let daily_warn_threshold = self.config.daily_limit_usd * warn_threshold;
        let monthly_warn_threshold = self.config.monthly_limit_usd * warn_threshold;

        if projected_daily >= daily_warn_threshold {
            return Ok(BudgetCheck::Warning {
                current_usd: daily_cost,
                limit_usd: self.config.daily_limit_usd,
                period: UsagePeriod::Day,
            });
        }

        if projected_monthly >= monthly_warn_threshold {
            return Ok(BudgetCheck::Warning {
                current_usd: monthly_cost,
                limit_usd: self.config.monthly_limit_usd,
                period: UsagePeriod::Month,
            });
        }

        Ok(BudgetCheck::Allowed)
    }

    /// Record a usage event.
    pub fn record_usage(&self, usage: TokenUsage) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        if !usage.cost_usd.is_finite() || usage.cost_usd < 0.0 {
            return Err(anyhow!(
                "Token usage cost must be a finite, non-negative value"
            ));
        }

        let record = CostRecord::new(&self.session_id, usage);

        // Persist first for durability guarantees.
        {
            let mut storage = self.lock_storage();
            storage.add_record(record.clone())?;
        }

        // Then update in-memory session snapshot.
        let mut session_costs = self.lock_session_costs();
        session_costs.push(record);

        Ok(())
    }

    /// Get the current cost summary.
    pub fn get_summary(&self) -> Result<CostSummary> {
        let (daily_cost, monthly_cost) = {
            let mut storage = self.lock_storage();
            storage.get_aggregated_costs()?
        };

        let session_costs = self.lock_session_costs();
        let session_cost: f64 = session_costs
            .iter()
            .map(|record| record.usage.cost_usd)
            .sum();
        let total_tokens: u64 = session_costs
            .iter()
            .map(|record| record.usage.total_tokens)
            .sum();
        let request_count = session_costs.len();
        let by_model = build_session_model_stats(&session_costs);

        Ok(CostSummary {
            session_cost_usd: session_cost,
            daily_cost_usd: daily_cost,
            monthly_cost_usd: monthly_cost,
            total_tokens,
            request_count,
            by_model,
        })
    }

    /// Get the daily cost for a specific date.
    pub fn get_daily_cost(&self, date: NaiveDate) -> Result<f64> {
        let storage = self.lock_storage();
        storage.get_cost_for_date(date)
    }

    /// Get the monthly cost for a specific month.
    pub fn get_monthly_cost(&self, year: i32, month: u32) -> Result<f64> {
        let storage = self.lock_storage();
        storage.get_cost_for_month(year, month)
    }
}

fn resolve_storage_path(workspace_dir: &Path) -> Result<PathBuf> {
    let state_dir = workspace_dir.join("state");
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create directory {}", state_dir.display()))?;
    Ok(state_dir.join("costs.db"))
}

fn build_session_model_stats(session_costs: &[CostRecord]) -> HashMap<String, ModelStats> {
    let mut by_model: HashMap<String, ModelStats> = HashMap::new();

    for record in session_costs {
        let entry = by_model
            .entry(record.usage.model.clone())
            .or_insert_with(|| ModelStats {
                model: record.usage.model.clone(),
                cost_usd: 0.0,
                total_tokens: 0,
                request_count: 0,
            });

        entry.cost_usd += record.usage.cost_usd;
        entry.total_tokens += record.usage.total_tokens;
        entry.request_count += 1;
    }

    by_model
}

/// SQLite schema for cost_records.
const COST_RECORDS_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS cost_records (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        model TEXT NOT NULL,
        input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        total_tokens INTEGER NOT NULL,
        cost_usd REAL NOT NULL,
        timestamp TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_cost_records_timestamp ON cost_records(timestamp);
    CREATE INDEX IF NOT EXISTS idx_cost_records_date ON cost_records(date(timestamp));
";

/// Persistent storage for cost records (SQLite).
struct CostStorage {
    path: PathBuf,
}

impl CostStorage {
    fn open_conn(&self) -> Result<rusqlite::Connection> {
        rusqlite::Connection::open(&self.path)
            .with_context(|| format!("Failed to open cost database at {}", self.path.display()))
    }

    /// Create or open SQLite cost storage. Migrates from JSONL if present.
    fn new(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let conn = rusqlite::Connection::open(&path)
            .with_context(|| format!("Failed to open cost database at {}", path.display()))?;
        conn.execute_batch(COST_RECORDS_SCHEMA)
            .with_context(|| "Failed to create cost_records schema")?;
        drop(conn);

        let storage = Self { path };

        // Migrate from legacy costs.jsonl if it exists (same directory, previous default).
        let jsonl_path = storage
            .path
            .parent()
            .map(|p| p.join("costs.jsonl"))
            .unwrap_or_else(|| PathBuf::from("costs.jsonl"));
        if jsonl_path.exists() {
            storage.migrate_from_jsonl(&jsonl_path)?;
        }

        Ok(storage)
    }

    fn migrate_from_jsonl(&self, jsonl_path: &Path) -> Result<()> {
        let file = File::open(jsonl_path)
            .with_context(|| format!("Failed to open legacy cost file {}", jsonl_path.display()))?;
        let reader = BufReader::new(file);
        let mut migrated = 0u64;
        for line in reader.lines() {
            let raw = line.with_context(|| "Failed to read legacy cost line")?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<CostRecord>(trimmed) {
                self.insert_record(&record)?;
                migrated += 1;
            }
        }
        if migrated > 0 {
            fs::remove_file(jsonl_path).unwrap_or_else(|e| {
                tracing::warn!("Could not remove legacy costs.jsonl after migration: {e}");
            });
            tracing::info!("Migrated {migrated} cost records from JSONL to SQLite");
        }
        Ok(())
    }

    fn insert_record(&self, record: &CostRecord) -> Result<()> {
        let conn = self.open_conn()?;
        let ts = record.usage.timestamp.to_rfc3339();
        conn.execute(
            "INSERT INTO cost_records (id, session_id, model, input_tokens, output_tokens, total_tokens, cost_usd, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.id,
                record.session_id,
                record.usage.model,
                record.usage.input_tokens as i64,
                record.usage.output_tokens as i64,
                record.usage.total_tokens as i64,
                record.usage.cost_usd,
                ts,
            ],
        )
        .with_context(|| "Failed to insert cost record")?;
        Ok(())
    }

    /// Add a new record.
    fn add_record(&mut self, record: CostRecord) -> Result<()> {
        self.insert_record(&record)
    }

    /// Get aggregated costs for current day and month (UTC).
    fn get_aggregated_costs(&mut self) -> Result<(f64, f64)> {
        let conn = self.open_conn()?;
        let now = Utc::now();
        let today = now.date_naive().format("%Y-%m-%d").to_string();
        let year_month = (now.year(), now.month());

        let daily: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE date(timestamp) = ?1",
            params![&today],
            |row| row.get(0),
        ).unwrap_or(0.0);
        let monthly: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE strftime('%Y', timestamp) = ?1 AND strftime('%m', timestamp) = ?2",
            params![year_month.0.to_string(), format!("{:02}", year_month.1)],
            |row| row.get(0),
        ).unwrap_or(0.0);
        Ok((daily, monthly))
    }

    /// Get cost for a specific date.
    fn get_cost_for_date(&self, date: NaiveDate) -> Result<f64> {
        let conn = self.open_conn()?;
        let date_str = date.format("%Y-%m-%d").to_string();
        let cost: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE date(timestamp) = ?1",
            params![date_str],
            |row| row.get(0),
        ).with_context(|| format!("Failed to get cost for date {date_str}"))?;
        Ok(cost)
    }

    /// Get cost for a specific month.
    fn get_cost_for_month(&self, year: i32, month: u32) -> Result<f64> {
        let conn = self.open_conn()?;
        let cost: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records WHERE strftime('%Y', timestamp) = ?1 AND strftime('%m', timestamp) = ?2",
            params![year.to_string(), format!("{:02}", month)],
            |row| row.get(0),
        ).with_context(|| format!("Failed to get cost for {year}-{month:02}"))?;
        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use tempfile::TempDir;

    fn enabled_config() -> CostConfig {
        CostConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn cost_tracker_initialization() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert!(!tracker.session_id().is_empty());
    }

    #[test]
    fn budget_check_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: false,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        let check = tracker.check_budget(1000.0).unwrap();
        assert!(matches!(check, BudgetCheck::Allowed));
    }

    #[test]
    fn record_usage_and_get_summary() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        let usage = TokenUsage::new("test/model", 1000, 500, 1.0, 2.0);
        tracker.record_usage(usage).unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.request_count, 1);
        assert!(summary.session_cost_usd > 0.0);
        assert_eq!(summary.by_model.len(), 1);
    }

    #[test]
    fn budget_exceeded_daily_limit() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            daily_limit_usd: 0.01, // Very low limit
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();

        // Record a usage that exceeds the limit
        let usage = TokenUsage::new("test/model", 10000, 5000, 1.0, 2.0); // ~0.02 USD
        tracker.record_usage(usage).unwrap();

        let check = tracker.check_budget(0.01).unwrap();
        assert!(matches!(check, BudgetCheck::Exceeded { .. }));
    }

    #[test]
    fn summary_by_model_is_session_scoped() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let jsonl_path = state_dir.join("costs.jsonl");

        let old_record = CostRecord::new(
            "old-session",
            TokenUsage::new("legacy/model", 500, 500, 1.0, 1.0),
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&old_record).unwrap()).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("session/model", 1000, 1000, 1.0, 1.0))
            .unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.by_model.len(), 1);
        assert!(summary.by_model.contains_key("session/model"));
        assert!(!summary.by_model.contains_key("legacy/model"));
    }

    #[test]
    fn malformed_lines_are_ignored_while_loading() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let jsonl_path = state_dir.join("costs.jsonl");

        let valid_usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
        let valid_record = CostRecord::new("session-a", valid_usage.clone());

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&valid_record).unwrap()).unwrap();
        writeln!(file, "not-a-json-line").unwrap();
        writeln!(file).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let today_cost = tracker.get_daily_cost(Utc::now().date_naive()).unwrap();
        assert!((today_cost - valid_usage.cost_usd).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_budget_estimate_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        let err = tracker.check_budget(f64::NAN).unwrap_err();
        assert!(err
            .to_string()
            .contains("Estimated cost must be a finite, non-negative value"));
    }
}
