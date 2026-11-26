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
const CURRENT_SCHEMA_VERSION: i32 = 4;
pub const ANTIGRAVITY_REPAIR_VERSION: i32 = 4;
const ANTIGRAVITY_REPAIR_KEY: &str = "antigravity_repair_version";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let store = Self { path: path.into() };
        let mut db = store.connect()?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
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
                input_at_ns INTEGER,
                cache_read_at_ns INTEGER,
                output_at_ns INTEGER,
                PRIMARY KEY(provider,session,response)
            );
            CREATE INDEX IF NOT EXISTS usage_date ON usage(provider,at);
            CREATE TABLE IF NOT EXISTS cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));
            CREATE TABLE IF NOT EXISTS antigravity_repair_tasks(
                kind TEXT NOT NULL,
                target TEXT PRIMARY KEY,
                fingerprint TEXT,
                max_pos INTEGER,
                done INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        migrate_usage_schema(&mut db)?;
        Ok(store)
    }
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection::open(&self.path)?)
    }
    pub fn antigravity_repair_pending(&self) -> Result<bool> {
        let ver: Option<String> = self
            .connect()?
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [ANTIGRAVITY_REPAIR_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let parsed = ver.and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        Ok(parsed < ANTIGRAVITY_REPAIR_VERSION)
    }
    pub fn mark_antigravity_repaired(&self) -> Result<()> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
            rusqlite::params![ANTIGRAVITY_REPAIR_KEY, ANTIGRAVITY_REPAIR_VERSION.to_string()],
        )?;
        tx.execute("DELETE FROM antigravity_repair_tasks WHERE done=1", [])?;
        tx.commit()?;
        Ok(())
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
        (
            "input_at_ns",
            "ALTER TABLE usage ADD COLUMN input_at_ns INTEGER",
        ),
        (
            "cache_read_at_ns",
            "ALTER TABLE usage ADD COLUMN cache_read_at_ns INTEGER",
        ),
        (
            "output_at_ns",
            "ALTER TABLE usage ADD COLUMN output_at_ns INTEGER",
        ),
    ] {
        if !existing.contains(col) {
            tx.execute(ddl, [])?;
        }
    }

    if version < 3 {
        // Detect pre-v3 databases (both v0 and v2 from 7d76bf1) that already hold Antigravity rows or cursors.
        let has_antigravity_data: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM usage WHERE provider='antigravity')
                 OR EXISTS(SELECT 1 FROM cursors WHERE provider='antigravity')",
            [],
            |r| r.get(0),
        )?;
        if has_antigravity_data {
            // Preserve Antigravity cursors so one-shot history repair only re-reads the proven
            // collected byte prefix [0..pos) even if Antigravity was disabled before upgrading.
            // Codex and Claude rows/cursors remain untouched.
            tx.execute(
                "UPDATE usage SET legacy=1 WHERE provider='antigravity'",
                [],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO antigravity_repair_tasks(kind, target, fingerprint, max_pos, done)
                 SELECT 'cursor', path, fingerprint, pos, 0
                 FROM cursors
                 WHERE provider='antigravity' AND pos >= 0",
                [],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                rusqlite::params![ANTIGRAVITY_REPAIR_KEY, "3"],
            )?;
        } else {
            tx.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                rusqlite::params![ANTIGRAVITY_REPAIR_KEY, ANTIGRAVITY_REPAIR_VERSION.to_string()],
            )?;
        }
    } else if version == 3 {
        // v3 DB from 969f12a / 9c9f61a: old cursors were already deleted by v3 migration.
        let rep: i32 = tx
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [ANTIGRAVITY_REPAIR_KEY],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        if rep >= 3 {
            tx.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                rusqlite::params![ANTIGRAVITY_REPAIR_KEY, ANTIGRAVITY_REPAIR_VERSION.to_string()],
            )?;
        } else {
            let has_legacy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM usage WHERE provider='antigravity' AND legacy=1)",
                [],
                |r| r.get(0),
            )?;
            if has_legacy {
                tx.execute(
                    "INSERT OR REPLACE INTO antigravity_repair_tasks(kind, target, fingerprint, max_pos, done)
                     SELECT DISTINCT 'session', session, NULL, NULL, 0
                     FROM usage
                     WHERE provider='antigravity' AND legacy=1",
                    [],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    rusqlite::params![ANTIGRAVITY_REPAIR_KEY, "3"],
                )?;
            } else {
                tx.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    rusqlite::params![ANTIGRAVITY_REPAIR_KEY, ANTIGRAVITY_REPAIR_VERSION.to_string()],
                )?;
            }
        }
    }
    tx.execute_batch(&format!("PRAGMA user_version={CURRENT_SCHEMA_VERSION};"))?;
    tx.commit()?;
    Ok(())
}
