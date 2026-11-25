use crate::discovery::Agent;
use anyhow::{ensure, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub struct Store {
    path: PathBuf,
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub agents: Vec<Agent>,
    pub enabled: Vec<String>,
    pub confirmed: bool,
}
const CURRENT_SCHEMA_VERSION: i32 = 2;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let store = Self { path: path.into() };
        let mut db = store.connect()?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS usage(
                provider TEXT,
                session TEXT,
                response TEXT,
                at INTEGER NOT NULL,
                tokens INTEGER NOT NULL,
                at_ns INTEGER,
                complete INTEGER NOT NULL DEFAULT 1,
                input_tokens INTEGER,
                cache_read_tokens INTEGER,
                output_tokens INTEGER,
                legacy INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(provider,session,response)
            );
            CREATE INDEX IF NOT EXISTS usage_date ON usage(provider,at);
            CREATE TABLE IF NOT EXISTS cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        migrate_usage_schema(&mut db)?;
        Ok(store)
    }
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection::open(&self.path)?)
    }
    pub fn settings(
        &self,
        force_scan: bool,
        scan: impl FnOnce() -> Vec<Agent>,
    ) -> Result<Settings> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx
            .query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0))
            .optional()?;
        let mut state = match raw {
            Some(raw) => {
                let mut s: Settings = serde_json::from_str(&raw)?;
                if force_scan {
                    s.agents = scan()
                }
                s
            }
            None => Settings {
                agents: scan(),
                enabled: vec![],
                confirmed: false,
            },
        };
        state
            .enabled
            .retain(|id| state.agents.iter().any(|agent| &agent.id == id));
        tx.execute(
            "INSERT OR REPLACE INTO settings VALUES (1,?1)",
            [serde_json::to_string(&state)?],
        )?;
        tx.commit()?;
        Ok(state)
    }
    pub fn select(&self, ids: Vec<String>) -> Result<Settings> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let data: String =
            tx.query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0))?;
        let mut state: Settings = serde_json::from_str(&data)?;
        for id in &ids {
            ensure!(
                state
                    .agents
                    .iter()
                    .any(|a| a.id == *id && (a.status != "missing" || state.enabled.contains(id))),
                "请先重新扫描以发现该工具"
            )
        }
        state.enabled = ids;
        state.confirmed = true;
        tx.execute(
            "UPDATE settings SET data=?1 WHERE id=1",
            [serde_json::to_string(&state)?],
        )?;
        tx.commit()?;
        Ok(state)
    }
    pub fn enabled_agent(&self, id: &str) -> Result<Agent> {
        let raw: String =
            self.connect()?
                .query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0))?;
        let state: Settings = serde_json::from_str(&raw)?;
        ensure!(
            state.confirmed && state.enabled.iter().any(|p| p == id),
            "平台未启用"
        );
        state
            .agents
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| anyhow::anyhow!("未知平台"))
    }
    pub fn cached_quota(&self) -> Result<Option<crate::quota::Quota>> {
        self.enabled_agent("codex")?;
        let data: Option<String> = self
            .connect()?
            .query_row("SELECT data FROM last_quota WHERE id=1", [], |r| r.get(0))
            .optional()?;
        Ok(data.map(|s| serde_json::from_str(&s)).transpose()?)
    }
    pub fn save_quota(&self, snapshot: &crate::quota::Quota) -> Result<()> {
        self.enabled_agent("codex")?;
        self.connect()?.execute(
            "INSERT OR REPLACE INTO last_quota VALUES (1,?1)",
            [serde_json::to_string(snapshot)?],
        )?;
        Ok(())
    }
}

fn migrate_usage_schema(db: &mut Connection) -> Result<()> {
    let version: i32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut existing = std::collections::HashSet::new();
    {
        let mut stmt = tx.prepare("PRAGMA table_info(usage)")?;
        let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for col in cols {
            existing.insert(col?);
        }
    }
    let upgraded_from_legacy = !existing.contains("at_ns") || !existing.contains("complete");
    for (col, ddl) in [
        ("at_ns", "ALTER TABLE usage ADD COLUMN at_ns INTEGER"),
        (
            "complete",
            "ALTER TABLE usage ADD COLUMN complete INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "input_tokens",
            "ALTER TABLE usage ADD COLUMN input_tokens INTEGER",
        ),
        (
            "cache_read_tokens",
            "ALTER TABLE usage ADD COLUMN cache_read_tokens INTEGER",
        ),
        (
            "output_tokens",
            "ALTER TABLE usage ADD COLUMN output_tokens INTEGER",
        ),
        (
            "legacy",
            "ALTER TABLE usage ADD COLUMN legacy INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if !existing.contains(col) {
            tx.execute(ddl, [])?;
        }
    }
    if upgraded_from_legacy {
        // Only mark pre-existing Antigravity rows as legacy and reset Antigravity cursors so
        // on-disk transcripts can backfill sub-second precision and cache completeness without
        // touching Codex or Claude cursors/history.
        tx.execute(
            "UPDATE usage SET legacy=1 WHERE provider='antigravity'",
            [],
        )?;
        tx.execute("DELETE FROM cursors WHERE provider='antigravity'", [])?;
    }
    tx.execute_batch(&format!("PRAGMA user_version={CURRENT_SCHEMA_VERSION};"))?;
    tx.commit()?;
    Ok(())
}
