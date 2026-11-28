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
const CURRENT_SCHEMA_VERSION: i32 = 6;
pub const ANTIGRAVITY_REPAIR_VERSION: i32 = 5;
const ANTIGRAVITY_REPAIR_KEY: &str = "antigravity_repair_version";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let store = Self { path: path.into() };
        let mut db = store.connect()?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS quota_snapshots(
                provider TEXT NOT NULL,
                account_key TEXT NOT NULL,
                checked_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY(provider, account_key)
            );
            CREATE INDEX IF NOT EXISTS quota_snapshots_provider_recent
                ON quota_snapshots(provider, updated_at DESC, checked_at DESC);
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
    pub fn antigravity_repair_version(&self) -> Result<i32> {
        let ver: Option<String> = self
            .connect()?
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [ANTIGRAVITY_REPAIR_KEY],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ver.and_then(|s| s.parse::<i32>().ok()).unwrap_or(0))
    }
    pub fn antigravity_repair_pending(&self) -> Result<bool> {
        Ok(self.antigravity_repair_version()? < ANTIGRAVITY_REPAIR_VERSION)
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
        // Surface platforms that were removed from disk (neither installed nor log dir)
        // when SQLite still retains historical usage rows for them.
        let mut db_providers = std::collections::HashSet::new();
        {
            let mut stmt = tx.prepare("SELECT DISTINCT provider FROM usage")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                db_providers.insert(row?);
            }
        }
        for agent in &mut state.agents {
            if agent.status == "missing" && db_providers.contains(&agent.id) {
                agent.status = "history".into();
            } else if agent.status == "history" && !db_providers.contains(&agent.id) {
                agent.status = "missing".into();
            }
        }
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
                state.agents.iter().any(|a| {
                    a.id == *id
                        && ((a.status != "missing" && a.status != "history")
                            || state.enabled.contains(id))
                }),
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
        self.cached_quota_for("codex", None)
    }
    pub fn cached_quota_for(
        &self,
        provider: &str,
        account: Option<&str>,
    ) -> Result<Option<crate::quota::Quota>> {
        self.enabled_agent(provider)?;
        let db = self.connect()?;
        match account {
            Some(acc) => {
                for key in quota_lookup_keys(acc) {
                    if let Some(raw) = db
                        .query_row(
                            "SELECT data FROM quota_snapshots WHERE provider=?1 AND account_key=?2",
                            rusqlite::params![provider, key],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()?
                    {
                        let q = deserialize_stored_quota(provider, &raw)?;
                        if quota_has_effective_data(&q) {
                            return Ok(Some(q));
                        }
                    }
                }
                Ok(None)
            }
            None => {
                let mut stmt = db.prepare(
                    "SELECT data FROM quota_snapshots
                     WHERE provider=?1
                     ORDER BY checked_at DESC, updated_at DESC",
                )?;
                let rows = stmt.query_map([provider], |r| r.get::<_, String>(0))?;
                for row in rows {
                    let q = deserialize_stored_quota(provider, &row?)?;
                    if quota_has_effective_data(&q) {
                        return Ok(Some(q));
                    }
                }
                if provider == "codex" {
                    let legacy: Option<String> = db
                        .query_row("SELECT data FROM last_quota WHERE id=1", [], |r| r.get(0))
                        .optional()?;
                    if let Some(raw) = legacy {
                        let q = normalize_legacy_codex_quota(&raw)?;
                        if quota_has_effective_data(&q) {
                            return Ok(Some(q));
                        }
                    }
                }
                Ok(None)
            }
        }
    }
    pub fn list_quota_snapshots(&self, provider: &str) -> Result<Vec<crate::quota::Quota>> {
        self.enabled_agent(provider)?;
        let db = self.connect()?;
        let mut stmt = db.prepare(
            "SELECT data FROM quota_snapshots
             WHERE provider=?1
             ORDER BY checked_at DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map([provider], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let q = deserialize_stored_quota(provider, &row?)?;
            if quota_has_effective_data(&q) {
                out.push(q);
            }
        }
        Ok(out)
    }
    pub fn save_quota(&self, snapshot: &crate::quota::Quota) -> Result<()> {
        let provider = if snapshot.provider.trim().is_empty() {
            "codex"
        } else {
            snapshot.provider.as_str()
        };
        self.save_quota_for(provider, snapshot)
    }
    pub fn save_quota_for(
        &self,
        provider: &str,
        snapshot: &crate::quota::Quota,
    ) -> Result<()> {
        let _ = self.merge_quota_for(provider, snapshot)?;
        Ok(())
    }
    /// Merges a quota snapshot into `quota_snapshots` keyed by `(provider, stable account key)`.
    /// Empty payloads (`windows` empty and `ai_credits` absent) are rejected BEFORE account keying
    /// so an anonymous `{}` snapshot never creates a phantom row or hides a prior valid snapshot.
    /// Returns `(effective_snapshot, updated, note)`.
    pub fn merge_quota_for(
        &self,
        provider: &str,
        snapshot: &crate::quota::Quota,
    ) -> Result<(Option<crate::quota::Quota>, bool, Option<String>)> {
        self.enabled_agent(provider)?;
        let mut stored = snapshot.clone();
        stored.provider = provider.to_string();
        if stored.source.trim().is_empty() && provider == "codex" {
            stored.source = "Codex CLI app-server".into();
        }
        if stored.source_kind.trim().is_empty() {
            stored.source_kind = if provider == "codex" {
                "live_query".into()
            } else {
                "local_snapshot".into()
            };
        }
        let account_key = quota_snapshot_account_key(&stored);
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Check payload effectiveness BEFORE per-account insert/merge so `{}` never crowds out valid data.
        if !quota_has_effective_data(&stored) {
            if let Some(prev) = latest_effective_quota_tx(&tx, provider, Some(&account_key))?
                .or(latest_effective_quota_tx(&tx, provider, None)?)
            {
                tx.commit()?;
                return Ok((
                    Some(prev),
                    false,
                    Some("新载荷未包含有效额度数据，已保留上次有效快照".into()),
                ));
            }
            tx.commit()?;
            return Ok((None, false, Some("暂无有效额度快照".into())));
        }

        let existing_raw: Option<String> = tx
            .query_row(
                "SELECT data FROM quota_snapshots WHERE provider=?1 AND account_key=?2",
                rusqlite::params![provider, account_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(raw) = existing_raw {
            let existing = deserialize_stored_quota(provider, &raw)?;
            if quota_has_effective_data(&existing) {
                if stored.checked_at < existing.checked_at {
                    tx.commit()?;
                    return Ok((
                        Some(existing),
                        false,
                        Some("检测到更早时间的旧快照，已保留较新快照".into()),
                    ));
                }
                if existing == stored {
                    tx.commit()?;
                    return Ok((
                        Some(existing),
                        false,
                        Some("未检测到更新的快照，以下为上次采集结果".into()),
                    ));
                }
            }
        }
        let encoded = serde_json::to_string(&stored)?;
        let updated_at = chrono::Utc::now().timestamp();
        tx.execute(
            "INSERT OR REPLACE INTO quota_snapshots(provider, account_key, checked_at, updated_at, data)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![provider, account_key, stored.checked_at, updated_at, encoded],
        )?;
        if provider == "codex" {
            tx.execute("INSERT OR REPLACE INTO last_quota VALUES (1,?1)", [&encoded])?;
        }
        tx.commit()?;
        Ok((Some(stored), true, None))
    }
}

fn quota_has_effective_data(q: &crate::quota::Quota) -> bool {
    !q.windows.is_empty() || q.ai_credits.is_some()
}

fn latest_effective_quota_tx(
    tx: &rusqlite::Transaction<'_>,
    provider: &str,
    account_key: Option<&str>,
) -> Result<Option<crate::quota::Quota>> {
    if let Some(key) = account_key {
        let raw: Option<String> = tx
            .query_row(
                "SELECT data FROM quota_snapshots WHERE provider=?1 AND account_key=?2",
                rusqlite::params![provider, key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(raw) = raw {
            let q = deserialize_stored_quota(provider, &raw)?;
            if quota_has_effective_data(&q) {
                return Ok(Some(q));
            }
        }
        return Ok(None);
    }
    let mut stmt = tx.prepare(
        "SELECT data FROM quota_snapshots
         WHERE provider=?1
         ORDER BY checked_at DESC, updated_at DESC",
    )?;
    let rows = stmt.query_map([provider], |r| r.get::<_, String>(0))?;
    for row in rows {
        let q = deserialize_stored_quota(provider, &row?)?;
        if quota_has_effective_data(&q) {
            return Ok(Some(q));
        }
    }
    Ok(None)
}

fn quota_legacy_account_key(account: &str) -> String {
    let trimmed = account.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        "_default".into()
    } else {
        trimmed
    }
}

fn quota_snapshot_account_key(snapshot: &crate::quota::Quota) -> String {
    if let Some(id) = snapshot
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("id:{}", id.to_ascii_lowercase());
    }
    quota_legacy_account_key(&snapshot.account)
}

fn quota_lookup_keys(account: &str) -> Vec<String> {
    let trimmed = account.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return vec!["_default".into()];
    }
    if trimmed.starts_with("id:") {
        vec![trimmed]
    } else if trimmed.starts_with("sha256:") {
        vec![format!("id:{trimmed}"), trimmed]
    } else {
        vec![format!("id:{trimmed}"), trimmed]
    }
}

fn deserialize_stored_quota(provider: &str, raw: &str) -> Result<crate::quota::Quota> {
    let mut q: crate::quota::Quota = serde_json::from_str(raw)?;
    if q.provider.trim().is_empty() {
        q.provider = provider.to_string();
    }
    if q.source.trim().is_empty() && provider == "codex" {
        q.source = "Codex CLI app-server".into();
    }
    if q.source_kind.trim().is_empty() {
        q.source_kind = if provider == "codex" {
            "live_query".into()
        } else {
            "local_snapshot".into()
        };
    }
    Ok(q)
}

/// Only used for migrating pre-v6 Codex `last_quota(id=1)` rows that predated `remaining_percent`.
fn normalize_legacy_codex_quota(raw: &str) -> Result<crate::quota::Quota> {
    let val: serde_json::Value = serde_json::from_str(raw)?;
    let mut q = deserialize_stored_quota("codex", raw)?;
    if let Some(arr) = val.get("windows").and_then(|v| v.as_array()) {
        for (w, raw_w) in q.windows.iter_mut().zip(arr.iter()) {
            let has_explicit_pct = raw_w.get("remaining_percent").is_some();
            let has_used = raw_w.get("used").and_then(|v| v.as_f64()).is_some();
            if !has_explicit_pct
                && w.remaining_percent.is_none()
                && w.remaining_amount.is_none()
                && !w.disabled
                && has_used
            {
                w.remaining_percent = Some((100.0 - w.used).clamp(0.0, 100.0));
            }
        }
    }
    Ok(q)
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
        // Detect pre-v3 databases (v0, 7d76bf1 v2, 7b1ecd9 v2) that already hold Antigravity rows or cursors.
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
                rusqlite::params![ANTIGRAVITY_REPAIR_KEY, "4"],
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
                    rusqlite::params![ANTIGRAVITY_REPAIR_KEY, "4"],
                )?;
            } else {
                tx.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    rusqlite::params![ANTIGRAVITY_REPAIR_KEY, ANTIGRAVITY_REPAIR_VERSION.to_string()],
                )?;
            }
        }
    } else if version == 4 {
        // v4 DB from 02c6166:
        // Keep any unfinished tasks intact, and re-queue sessions left purely `legacy=1`
        // (such as `7b1ecd9 -> 969f12a -> 02c6166` where 02c6166 only knew 7d76bf1 rules).
        // Sessions already containing `legacy=0` rows are left untouched to avoid deleting user history
        // without provenance.
        tx.execute(
            "INSERT OR IGNORE INTO antigravity_repair_tasks(kind, target, fingerprint, max_pos, done)
             SELECT DISTINCT 'session', u.session, NULL, NULL, 0
             FROM usage u
             WHERE u.provider='antigravity' AND u.legacy=1
               AND NOT EXISTS(
                   SELECT 1 FROM usage u2
                   WHERE u2.provider='antigravity' AND u2.session=u.session AND u2.legacy=0
               )
               AND NOT EXISTS(
                   SELECT 1 FROM antigravity_repair_tasks t
                   WHERE t.done=0
                     AND (
                         (t.kind='session' AND t.target=u.session)
                         OR (t.kind='cursor' AND (
                             t.target LIKE '%/' || u.session || '/.system_generated/logs/transcript.jsonl'
                             OR t.target LIKE '%\\' || u.session || '\\.system_generated\\logs\\transcript.jsonl'
                         ))
                     )
               )",
            [],
        )?;
        let has_pending_tasks: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM antigravity_repair_tasks WHERE done=0)",
            [],
            |r| r.get(0),
        )?;
        let target_ver = if has_pending_tasks {
            "4".to_string()
        } else {
            ANTIGRAVITY_REPAIR_VERSION.to_string()
        };
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
            rusqlite::params![ANTIGRAVITY_REPAIR_KEY, target_ver],
        )?;
    }
    if version < 6 {
        if let Some(legacy_codex) = tx
            .query_row("SELECT data FROM last_quota WHERE id=1", [], |r| r.get::<_, String>(0))
            .optional()?
        {
            if let Ok(q) = normalize_legacy_codex_quota(&legacy_codex) {
                let encoded = serde_json::to_string(&q)?;
                let acc_key = quota_snapshot_account_key(&q);
                tx.execute(
                    "INSERT OR IGNORE INTO quota_snapshots(provider, account_key, checked_at, updated_at, data)
                     VALUES('codex', ?1, ?2, ?2, ?3)",
                    rusqlite::params![acc_key, q.checked_at, encoded],
                )?;
            }
        }
    }
    tx.execute_batch(&format!("PRAGMA user_version={CURRENT_SCHEMA_VERSION};"))?;
    tx.commit()?;
    Ok(())
}
