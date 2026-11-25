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
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let store = Self { path: path.into() };
        store.connect()?.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, PRIMARY KEY(provider,session,response));
            CREATE INDEX IF NOT EXISTS usage_date ON usage(provider,at);
            CREATE TABLE IF NOT EXISTS cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));")?;
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
