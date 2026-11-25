use crate::store::Store;
use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
};
use walkdir::WalkDir;

#[derive(Serialize)]
pub struct Usage {
    pub today: Option<i64>,
    pub latest: Option<i64>,
    pub latest_at: Option<i64>,
}

#[derive(Default)]
struct Changes {
    all: bool,
    paths: BTreeSet<PathBuf>,
}

// Recheck active rollouts on every UI poll even when the filesystem drops a notification.
const HOT_LOG_LIMIT: usize = 16;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Monitor {
    watcher: Option<RecommendedWatcher>,
    recent: BTreeSet<(SystemTime, PathBuf)>,
    last_reconcile: Option<Instant>,
    changes: Arc<Mutex<Changes>>,
}

impl Monitor {
    pub fn update(&mut self, store: &Store, id: &str, root: &Path) -> Result<Usage> {
        store.enabled_agent(id)?;
        if !root.is_dir() {
            self.watcher = None;
            self.recent.clear();
            self.last_reconcile = None;
            return totals(store, id);
        }
        let reconcile = self
            .last_reconcile
            .is_none_or(|last| last.elapsed() >= RECONCILE_INTERVAL);
        if reconcile && self.watcher.is_none() {
            let queue = self.changes.clone();
            let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                let mut pending = queue.lock().unwrap();
                match event {
                    Ok(e) if e.need_rescan() => pending.all = true,
                    Ok(e)
                        if matches!(
                            e.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
                        ) =>
                    {
                        for path in e.paths {
                            if path.extension().is_some_and(|s| s == "jsonl") || path.is_dir() {
                                pending.paths.insert(path);
                            }
                        }
                        if pending.paths.len() > 1024 {
                            pending.paths.clear();
                            pending.all = true
                        }
                    }
                    Err(_) => pending.all = true,
                    _ => {}
                }
            });
            // Keep reading by cursor if the native watcher cannot subscribe.
            if let Ok(mut watcher) = watcher {
                if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                    self.watcher = Some(watcher);
                }
            }
        }
        let pending = std::mem::take(&mut *self.changes.lock().unwrap());
        self.sync(store, id, root, pending, reconcile)?;
        totals(store, id)
    }

    fn sync(
        &mut self,
        store: &Store,
        id: &str,
        root: &Path,
        pending: Changes,
        reconcile: bool,
    ) -> Result<()> {
        let full = reconcile || pending.all;
        let paths = if full {
            BTreeSet::from([root.to_path_buf()])
        } else {
            let mut paths = pending.paths;
            paths.extend(self.recent.iter().map(|(_, path)| path.clone()));
            paths
        };
        if paths.is_empty() {
            return Ok(());
        }

        let mut db = store.connect()?;
        for path in paths {
            if let Err(error) = import(&mut db, id, &path, &mut self.recent) {
                self.changes.lock().unwrap().all = true;
                return Err(error);
            }
        }
        if full {
            self.last_reconcile = Some(Instant::now());
        }
        Ok(())
    }
}

fn totals(store: &Store, id: &str) -> Result<Usage> {
    let db = store.connect()?;
    let latest: Option<(i64, i64)> = db
        .query_row(
            "SELECT at,tokens FROM usage WHERE provider=?1 ORDER BY at DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let today = chrono::Local::now().date_naive();
    let start = today
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
        .context("无法确定本地日期")?
        .timestamp();
    let sum: i64 = db.query_row(
        "SELECT COALESCE(SUM(tokens),0) FROM usage WHERE provider=?1 AND at>=?2",
        params![id, start],
        |r| r.get(0),
    )?;
    Ok(Usage {
        today: latest.map(|_| sum),
        latest: latest.map(|(_, t)| t),
        latest_at: latest.map(|(t, _)| t),
    })
}

fn import(
    db: &mut rusqlite::Connection,
    id: &str,
    root: &Path,
    recent: &mut BTreeSet<(SystemTime, PathBuf)>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            continue;
        }
        let path = entry.path();
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let modified = meta.modified()?;
        if recent.len() < HOT_LOG_LIMIT
            || recent
                .first()
                .is_some_and(|(oldest, _)| modified >= *oldest)
        {
            recent.retain(|(_, p)| p != path);
            recent.insert((modified, path.to_path_buf()));
            if recent.len() > HOT_LOG_LIMIT {
                recent.pop_first();
            }
        }
        let fingerprint = file_id(&meta);
        let key = path.to_string_lossy();
        let cursor: Option<(String, i64)> = db
            .query_row(
                "SELECT fingerprint,pos FROM cursors WHERE provider=?1 AND path=?2",
                params![id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut pos = cursor
            .filter(|(f, p)| f == &fingerprint && *p >= 0 && *p as u64 <= meta.len())
            .map_or(0, |(_, p)| p as u64);
        if pos == meta.len() {
            continue;
        }
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if file_id(&file.metadata()?) != fingerprint {
            continue;
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(pos))?;
        let tx = db.transaction()?;
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            let len = reader.read_until(b'\n', &mut bytes)?;
            if len == 0 || bytes.last() != Some(&b'\n') {
                break;
            }
            pos += len as u64;
            if let Ok(event) = serde_json::from_slice::<Value>(&bytes) {
                if let Some((session, response, at, tokens)) = record(id, &event) {
                    tx.execute("INSERT INTO usage VALUES(?1,?2,?3,?4,?5)
                        ON CONFLICT(provider,session,response) DO UPDATE SET at=excluded.at,tokens=excluded.tokens WHERE excluded.at>=usage.at",params![id,session,response,at,tokens])?;
                }
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO cursors VALUES(?1,?2,?3,?4)",
            params![id, key, fingerprint, i64::try_from(pos)?],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(meta: &std::fs::Metadata) -> String {
    format!("{:?}", meta.created().ok())
}

fn record<'a>(id: &str, event: &'a Value) -> Option<(&'a str, &'a str, i64, i64)> {
    let at = chrono::DateTime::parse_from_rfc3339(event["timestamp"].as_str()?)
        .ok()?
        .timestamp();
    match (id, event["type"].as_str()?) {
        ("codex", "token_usage_record") => {
            let p = &event["payload"];
            Some((
                p["session_id"].as_str()?,
                p["response_id"].as_str()?,
                at,
                p["usage"]["total_tokens"].as_i64()?,
            ))
        }
        ("claude", "assistant") => {
            let m = &event["message"];
            let u = &m["usage"];
            let total = u["input_tokens"].as_i64()?
                + u["output_tokens"].as_i64()?
                + u["cache_creation_input_tokens"].as_i64().unwrap_or(0)
                + u["cache_read_input_tokens"].as_i64().unwrap_or(0);
            Some((event["sessionId"].as_str()?, m["id"].as_str()?, at, total))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Agent;
    use serde_json::json;
    use std::{fs, io::Write};

    #[test]
    fn catches_up_from_partial_index_without_native_events() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("state.db"))?;
        store.settings(false, || {
            vec![Agent {
                id: "codex".into(),
                name: "Codex".into(),
                binary: None,
                status: "installed".into(),
            }]
        })?;
        store.select(vec!["codex".into()])?;
        let root = dir.path().join("sessions/2026/11/25");
        fs::create_dir_all(&root)?;
        let path = root.join("rollout.jsonl");
        fs::write(&path, "{\"type\":\"session_meta\"}\n")?;
        let mut monitor = Monitor::default();
        assert!(monitor.update(&store, "codex", &root)?.latest.is_none());
        // Keep the cursor established at discovery, then suppress all OS notifications.
        drop(monitor.watcher.take());
        *monitor.changes.lock().unwrap() = Changes::default();

        let mut log = fs::OpenOptions::new().append(true).open(&path)?;
        let today = chrono::Local::now().date_naive();
        let event = |response: i64, tokens: i64| {
            json!({
                "type":"token_usage_record",
                "timestamp":today.and_hms_opt(0,1,response as u32).unwrap()
                    .and_local_timezone(chrono::Local).earliest().unwrap().to_rfc3339(),
                "payload":{"session_id":"example","response_id":response.to_string(),"usage":{"total_tokens":tokens}}
            })
        };
        for (response, tokens) in [(1, 19895), (2, 19908), (3, 19926)] {
            writeln!(log, "{}", event(response, tokens))?;
            writeln!(
                log,
                "{}",
                json!({"type":"event_msg","payload":{"type":"token_count"}})
            )?;
        }
        for _ in 0..2 {
            let usage = monitor.update(&store, "codex", &root)?;
            assert_eq!(usage.today, Some(59729));
            assert_eq!(usage.latest, Some(19926));
        }

        // A missed create in an unobserved directory is recovered on reconciliation.
        let nested = root.join("other");
        fs::create_dir(&nested)?;
        fs::write(nested.join("new.jsonl"), format!("{}\n", event(4, 1)))?;
        monitor.last_reconcile = Some(Instant::now() - RECONCILE_INTERVAL);
        let usage = monitor.update(&store, "codex", &root)?;
        assert_eq!(usage.today, Some(59730));
        assert_eq!(usage.latest, Some(1));
        Ok(())
    }
}
