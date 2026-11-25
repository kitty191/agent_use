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

#[derive(Default)]
pub struct Monitor {
    watcher: Option<RecommendedWatcher>,
    changes: Arc<Mutex<Changes>>,
}

impl Monitor {
    pub fn update(&mut self, store: &Store, id: &str, root: &Path) -> Result<Usage> {
        store.enabled_agent(id)?;
        if !root.is_dir() {
            self.watcher = None;
            return totals(store, id);
        }
        if self.watcher.is_none() {
            let queue = self.changes.clone();
            let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
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
            })?;
            watcher.watch(root, RecursiveMode::Recursive)?;
            self.watcher = Some(watcher);
            self.changes.lock().unwrap().all = true;
        }
        let pending = std::mem::take(&mut *self.changes.lock().unwrap());
        let paths = if pending.all {
            BTreeSet::from([root.to_path_buf()])
        } else {
            pending.paths
        };
        if !paths.is_empty() {
            let mut db = store.connect()?;
            for path in paths {
                if let Err(error) = import(&mut db, id, &path) {
                    self.changes.lock().unwrap().all = true;
                    return Err(error);
                }
            }
        }
        totals(store, id)
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

fn import(db: &mut rusqlite::Connection, id: &str, root: &Path) -> Result<()> {
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
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        #[cfg(unix)]
        let fingerprint = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}", meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let fingerprint = format!("{:?}", meta.created().ok());
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
