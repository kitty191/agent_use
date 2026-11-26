use crate::store::Store;
use anyhow::{bail, Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, ErrorKind, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use walkdir::WalkDir;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Active,
    Waiting,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TokenParts {
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub output: Option<i64>,
}

#[derive(Serialize, Debug)]
pub struct Usage {
    pub today: Option<i64>,
    pub today_complete_steps: i64,
    pub today_incomplete_steps: i64,
    pub today_incomplete_known: Option<TokenParts>,
    pub today_legacy_steps: i64,
    pub today_legacy_tokens: Option<i64>,
    pub latest: Option<i64>,
    pub latest_at: Option<i64>,
    pub latest_at_ms: Option<i64>,
    pub latest_incomplete: bool,
    pub latest_parts: Option<TokenParts>,
    pub latest_legacy: bool,
    pub latest_legacy_tokens: Option<i64>,
    pub status: SourceStatus,
}

struct ParsedRecord {
    session: String,
    response: String,
    at: i64,
    at_ns: i64,
    tokens: i64,
    complete: bool,
    input: Option<i64>,
    cache_read: Option<i64>,
    output: Option<i64>,
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
    codex_day_dirs: BTreeMap<PathBuf, SystemTime>,
    antigravity_transcripts: BTreeMap<PathBuf, (SystemTime, u64)>,
    changes: Arc<Mutex<Changes>>,
}

impl Monitor {
    pub fn update(&mut self, store: &Store, id: &str, root: &Path) -> Result<Usage> {
        store.enabled_agent(id)?;
        match std::fs::metadata(root) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.reset_runtime();
                return totals(store, id, SourceStatus::Waiting);
            }
            Err(e) => {
                self.reset_runtime();
                return Err(e).with_context(|| format!("无法访问日志目录 {}", root.display()));
            }
            Ok(meta) if !meta.is_dir() => {
                self.reset_runtime();
                bail!("日志路径不是目录: {}", root.display());
            }
            Ok(_) => {}
        }
        // Verify directory readability so permission errors are surfaced instead of silently ignored.
        std::fs::read_dir(root)
            .with_context(|| format!("无法读取日志目录 {}", root.display()))?;

        let reconcile = self
            .last_reconcile
            .is_none_or(|last| last.elapsed() >= RECONCILE_INTERVAL);
        if reconcile && self.watcher.is_none() {
            let queue = self.changes.clone();
            let trace = cfg!(debug_assertions)
                && id == "codex"
                && std::env::var_os("AGENT_USE_TRACE_CODEX").is_some();
            let watch_root = root.to_path_buf();
            if trace {
                eprintln!("[usage-watch] root={}", watch_root.display());
            }
            let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                if trace {
                    match &event {
                        Ok(event) => {
                            let paths: Vec<_> = event
                                .paths
                                .iter()
                                .map(|p| p.strip_prefix(&watch_root).unwrap_or(p))
                                .collect();
                            eprintln!(
                                "[usage-watch] {:?} paths={paths:?} rescan={}",
                                event.kind,
                                event.need_rescan()
                            );
                        }
                        Err(error) => eprintln!("[usage-watch] {error}"),
                    }
                }
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
        let mut pending = std::mem::take(&mut *self.changes.lock().unwrap());
        let repairing_antigravity = id == "antigravity" && store.antigravity_repair_pending()?;
        if repairing_antigravity {
            // Force a complete check of all on-disk Antigravity transcripts; files already
            // repaired before an interruption have EOF cursors and are skipped cleanly.
            pending.all = true;
        }
        if id == "codex" {
            // A new rollout is not in the hot-file set yet. Directory mtime changes
            // on creation/rename even if the OS coalesces or drops the file event.
            let now = chrono::Utc::now();
            let dates = [
                now.with_timezone(&chrono::Local).date_naive(),
                now.date_naive(),
            ];
            let mut day_dirs = BTreeMap::new();
            for date in dates.into_iter().collect::<BTreeSet<_>>() {
                let day = root.join(date.format("%Y/%m/%d").to_string());
                if let Ok(stamp) = day.metadata().and_then(|m| m.modified()) {
                    if self.codex_day_dirs.get(&day) != Some(&stamp) {
                        pending.paths.insert(day.clone());
                    }
                    day_dirs.insert(day, stamp);
                }
            }
            self.codex_day_dirs = day_dirs;
        } else if id == "antigravity" {
            // Antigravity writes `<brain>/<conv-id>/.system_generated/logs/transcript.jsonl`.
            // Checking top-level conversation folders directly catches newly created hidden
            // log trees and same-second appends even if FSEvents drops nested notifications.
            let mut current = BTreeMap::new();
            for transcript in discover_antigravity_transcripts(root)? {
                match std::fs::metadata(&transcript) {
                    Ok(meta) => {
                        let sig = (meta.modified()?, meta.len());
                        if self.antigravity_transcripts.get(&transcript) != Some(&sig) {
                            pending.paths.insert(transcript.clone());
                        }
                        current.insert(transcript, sig);
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("无法读取日志文件 {}", transcript.display()))
                    }
                }
            }
            self.antigravity_transcripts = current;
        }
        self.sync(store, id, root, pending, reconcile)?;
        if repairing_antigravity {
            store.mark_antigravity_repaired()?;
        }
        totals(store, id, SourceStatus::Active)
    }

    fn reset_runtime(&mut self) {
        self.watcher = None;
        self.recent.clear();
        self.codex_day_dirs.clear();
        self.antigravity_transcripts.clear();
        self.last_reconcile = None;
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
            if let Err(error) = import(&mut db, id, root, &path, &mut self.recent) {
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

fn totals(store: &Store, id: &str, status: SourceStatus) -> Result<Usage> {
    let db = store.connect()?;
    // Latest step ordering:
    // 1. Exact source timestamp preserving full sub-second precision (at_ns, falling back to at * 1e9 for legacy rows)
    // 2. Deterministic total-order tie-breakers independent of import/file order:
    //    session DESC, then numeric step index DESC (or lexical response DESC).
    let latest: Option<(i64, Option<i64>, i64, i64, Option<i64>, Option<i64>, Option<i64>, i64)> =
        db.query_row(
            "SELECT at, at_ns, tokens, complete, input_tokens, cache_read_tokens, output_tokens, legacy
             FROM usage
             WHERE provider=?1
             ORDER BY
               COALESCE(at_ns, at * 1000000000) DESC,
               session DESC,
               CASE WHEN response GLOB '[0-9]*' AND response NOT GLOB '*[^0-9]*' THEN LENGTH(response) ELSE 0 END DESC,
               response DESC
             LIMIT 1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?;

    let today_date = chrono::Local::now().date_naive();
    let start = crate::history::local_day_start_sec(today_date, &chrono::Local)?;
    let next_day = today_date
        .succ_opt()
        .context("无法计算下一本地日期")?;
    let end = crate::history::local_day_start_sec(next_day, &chrono::Local)?;

    // Primary verified total strictly includes verified complete non-legacy rows only.
    let (today_complete_steps, today_complete_sum): (i64, i64) = db.query_row(
        "SELECT COUNT(*), COALESCE(SUM(tokens),0)
         FROM usage
         WHERE provider=?1 AND at>=?2 AND at<?3 AND complete=1 AND legacy=0",
        params![id, start, end],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let (
        today_incomplete_steps,
        inc_in_cnt,
        inc_in_sum,
        inc_cr_cnt,
        inc_cr_sum,
        inc_out_cnt,
        inc_out_sum,
    ): (i64, i64, i64, i64, i64, i64, i64) = db.query_row(
        "SELECT
            COUNT(*),
            COUNT(input_tokens), COALESCE(SUM(input_tokens), 0),
            COUNT(cache_read_tokens), COALESCE(SUM(cache_read_tokens), 0),
            COUNT(output_tokens), COALESCE(SUM(output_tokens), 0)
         FROM usage
         WHERE provider=?1 AND at>=?2 AND at<?3 AND complete=0 AND legacy=0",
        params![id, start, end],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    )?;

    let (today_legacy_steps, today_legacy_sum): (i64, i64) = db.query_row(
        "SELECT COUNT(*), COALESCE(SUM(tokens),0)
         FROM usage
         WHERE provider=?1 AND at>=?2 AND at<?3 AND legacy=1",
        params![id, start, end],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let has_any_verified: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage WHERE provider=?1 AND complete=1 AND legacy=0)",
        [id],
        |r| r.get(0),
    )?;

    // `today` sums verified complete non-legacy steps only.
    // - If verified steps exist today -> Some(sum) (including real 0).
    // - Else if today has only incomplete or legacy steps -> None (don't pretend partial/unverified is complete).
    // - Else if verified history exists on other days -> Some(0).
    // - Else -> None.
    let today = if today_complete_steps > 0 {
        Some(today_complete_sum)
    } else if today_incomplete_steps > 0 || today_legacy_steps > 0 {
        None
    } else if has_any_verified {
        Some(0)
    } else {
        None
    };

    let today_incomplete_known = if today_incomplete_steps > 0 {
        Some(TokenParts {
            input: (inc_in_cnt > 0).then_some(inc_in_sum),
            cache_read: (inc_cr_cnt > 0).then_some(inc_cr_sum),
            output: (inc_out_cnt > 0).then_some(inc_out_sum),
        })
    } else {
        None
    };

    let (
        latest_val,
        latest_at,
        latest_at_ms,
        latest_incomplete,
        latest_parts,
        latest_legacy,
        latest_legacy_tokens,
    ) = match latest {
        Some((at, at_ns, tokens, complete, inp, cr, out, legacy)) => {
            let is_legacy = legacy == 1;
            let is_verified_complete = !is_legacy && complete == 1;
            let is_incomplete = !is_legacy && complete == 0;
            let parts = if is_incomplete {
                Some(TokenParts {
                    input: inp,
                    cache_read: cr,
                    output: out,
                })
            } else {
                None
            };
            (
                is_verified_complete.then_some(tokens),
                Some(at),
                Some(at_ns.map_or(at * 1000, |ns| ns / 1_000_000)),
                is_incomplete,
                parts,
                is_legacy,
                is_legacy.then_some(tokens),
            )
        }
        None => (None, None, None, false, None, false, None),
    };

    Ok(Usage {
        today,
        today_complete_steps,
        today_incomplete_steps,
        today_incomplete_known,
        today_legacy_steps,
        today_legacy_tokens: (today_legacy_steps > 0).then_some(today_legacy_sum),
        latest: latest_val,
        latest_at,
        latest_at_ms,
        latest_incomplete,
        latest_parts,
        latest_legacy,
        latest_legacy_tokens,
        status,
    })
}

fn discover_antigravity_transcripts(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("无法读取日志目录 {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let transcript = path.join(".system_generated/logs/transcript.jsonl");
        match std::fs::metadata(&transcript) {
            Ok(meta) if meta.is_file() => found.push(transcript),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("无法访问日志文件 {}", transcript.display()))
            }
        }
    }
    found.sort();
    Ok(found)
}

fn antigravity_conversation_id(base_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base_root).ok()?;
    let parts: Vec<_> = rel.components().collect();
    if parts.len() != 4 {
        return None;
    }
    let [Component::Normal(conv), Component::Normal(sys), Component::Normal(logs), Component::Normal(file)] =
        parts.as_slice()
    else {
        return None;
    };
    if *sys == ".system_generated" && *logs == "logs" && *file == "transcript.jsonl" {
        Some(conv.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn import(
    db: &mut rusqlite::Connection,
    id: &str,
    base_root: &Path,
    target: &Path,
    recent: &mut BTreeSet<(SystemTime, PathBuf)>,
) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    if id == "antigravity" {
        let files = if target == base_root {
            discover_antigravity_transcripts(base_root)?
        } else if antigravity_conversation_id(base_root, target).is_some() {
            vec![target.to_path_buf()]
        } else if target.is_dir() {
            if let Ok(rel) = target.strip_prefix(base_root) {
                if let Some(Component::Normal(conv)) = rel.components().next() {
                    let transcript = base_root
                        .join(conv)
                        .join(".system_generated/logs/transcript.jsonl");
                    if transcript.is_file() {
                        vec![transcript]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            // Ignore transcript_full.jsonl, chunks/*.jsonl, and unrelated files.
            Vec::new()
        };
        for file_path in files {
            let conv_id = antigravity_conversation_id(base_root, &file_path);
            import_file(db, id, &file_path, conv_id.as_deref(), recent)?;
        }
        return Ok(());
    }

    for entry in WalkDir::new(target) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            continue;
        }
        import_file(db, id, entry.path(), None, recent)?;
    }
    Ok(())
}

fn merge_field(
    ex_val: Option<i64>,
    ex_ts: Option<i64>,
    inc_val: Option<i64>,
    inc_ts: i64,
) -> (Option<i64>, Option<i64>) {
    match (ex_val, inc_val) {
        (_, None) => (ex_val, ex_ts),
        (None, Some(v)) => (Some(v), Some(inc_ts)),
        (Some(ev), Some(iv)) => {
            if ex_ts.is_none_or(|ts| inc_ts >= ts) {
                (Some(iv), Some(inc_ts))
            } else {
                (Some(ev), ex_ts)
            }
        }
    }
}

fn upsert_antigravity_record(tx: &rusqlite::Transaction<'_>, rec: &ParsedRecord) -> Result<()> {
    type ExistingRow = (
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let existing: Option<ExistingRow> = tx
        .query_row(
            "SELECT at, at_ns, input_tokens, cache_read_tokens, output_tokens, legacy,
                    input_at_ns, cache_read_at_ns, output_at_ns
             FROM usage
             WHERE provider='antigravity' AND session=?1 AND response=?2",
            params![rec.session, rec.response],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .optional()?;

    let (at, at_ns, input, in_ts, cache_read, cr_ts, output, out_ts) = match existing {
        None | Some((_, _, _, _, _, 1, _, _, _)) => {
            // New row or legacy pre-v3 row being repaired from on-disk transcript: initialize cleanly.
            (
                rec.at,
                rec.at_ns,
                rec.input,
                rec.input.map(|_| rec.at_ns),
                rec.cache_read,
                rec.cache_read.map(|_| rec.at_ns),
                rec.output,
                rec.output.map(|_| rec.at_ns),
            )
        }
        Some((ex_at, ex_at_ns, ex_in, ex_cr, ex_out, _, ex_in_ts, ex_cr_ts, ex_out_ts)) => {
            let ex_ns = ex_at_ns.unwrap_or(ex_at.saturating_mul(1_000_000_000));
            // Attribution timestamp is always the earliest valid source timestamp observed for this step.
            let (keep_at, keep_ns) = if rec.at_ns < ex_ns {
                (rec.at, rec.at_ns)
            } else {
                (ex_at, ex_ns)
            };
            // Each token field independently adopts the value with the latest valid source timestamp.
            let (m_in, m_in_ts) =
                merge_field(ex_in, ex_in_ts.or_else(|| ex_in.map(|_| ex_ns)), rec.input, rec.at_ns);
            let (m_cr, m_cr_ts) = merge_field(
                ex_cr,
                ex_cr_ts.or_else(|| ex_cr.map(|_| ex_ns)),
                rec.cache_read,
                rec.at_ns,
            );
            let (m_out, m_out_ts) = merge_field(
                ex_out,
                ex_out_ts.or_else(|| ex_out.map(|_| ex_ns)),
                rec.output,
                rec.at_ns,
            );
            (
                keep_at, keep_ns, m_in, m_in_ts, m_cr, m_cr_ts, m_out, m_out_ts,
            )
        }
    };

    let complete = input.is_some() && cache_read.is_some() && output.is_some();
    let tokens = input
        .unwrap_or(0)
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(output.unwrap_or(0));

    tx.execute(
        "INSERT OR REPLACE INTO usage(
            provider,session,response,at,tokens,at_ns,complete,
            input_tokens,cache_read_tokens,output_tokens,legacy,
            input_at_ns,cache_read_at_ns,output_at_ns
         )
         VALUES('antigravity',?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11,?12)",
        params![
            rec.session,
            rec.response,
            at,
            tokens,
            at_ns,
            i64::from(complete),
            input,
            cache_read,
            output,
            in_ts,
            cr_ts,
            out_ts,
        ],
    )?;
    Ok(())
}

fn import_file(
    db: &mut rusqlite::Connection,
    id: &str,
    path: &Path,
    session_fallback: Option<&str>,
    recent: &mut BTreeSet<(SystemTime, PathBuf)>,
) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("无法读取日志文件 {}", path.display())),
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
        return Ok(());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("无法打开日志文件 {}", path.display())),
    };
    if file_id(&file.metadata()?) != fingerprint {
        return Ok(());
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
            if let Some(rec) = record(id, session_fallback, &event) {
                if id == "antigravity" {
                    upsert_antigravity_record(&tx, &rec)?;
                } else {
                    tx.execute(
                        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)
                         ON CONFLICT(provider,session,response) DO UPDATE SET
                            at=excluded.at,
                            at_ns=excluded.at_ns,
                            tokens=excluded.tokens,
                            complete=excluded.complete,
                            input_tokens=excluded.input_tokens,
                            cache_read_tokens=excluded.cache_read_tokens,
                            output_tokens=excluded.output_tokens,
                            legacy=0
                         WHERE COALESCE(excluded.at_ns, excluded.at * 1000000000)
                                >= COALESCE(usage.at_ns, usage.at * 1000000000)",
                        params![
                            id,
                            rec.session,
                            rec.response,
                            rec.at,
                            rec.tokens,
                            rec.at_ns,
                            i64::from(rec.complete),
                            rec.input,
                            rec.cache_read,
                            rec.output,
                        ],
                    )?;
                }
            }
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO cursors VALUES(?1,?2,?3,?4)",
        params![id, key, fingerprint, i64::try_from(pos)?],
    )?;
    tx.commit()?;
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

fn parse_rfc3339(raw: &str) -> Option<(i64, i64)> {
    let dt = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let at = dt.timestamp();
    let at_ns = dt.timestamp_nanos_opt()?;
    Some((at, at_ns))
}

fn optional_nonneg_i64(event: &Value, key: &str) -> Result<Option<i64>, ()> {
    match event.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().filter(|&n| n >= 0).map(Some).ok_or(()),
    }
}

fn record(id: &str, session_fallback: Option<&str>, event: &Value) -> Option<ParsedRecord> {
    match (id, event["type"].as_str()?) {
        ("codex", "token_usage_record") => {
            let (at, at_ns) = parse_rfc3339(event["timestamp"].as_str()?)?;
            let p = &event["payload"];
            let tokens = p["usage"]["total_tokens"].as_i64()?;
            Some(ParsedRecord {
                session: p["session_id"].as_str()?.to_string(),
                response: p["response_id"].as_str()?.to_string(),
                at,
                at_ns,
                tokens,
                complete: true,
                input: p["usage"]["input_tokens"].as_i64(),
                cache_read: p["usage"]["cached_input_tokens"].as_i64(),
                output: p["usage"]["output_tokens"].as_i64(),
            })
        }
        ("claude", "assistant") => {
            let (at, at_ns) = parse_rfc3339(event["timestamp"].as_str()?)?;
            let m = &event["message"];
            let u = &m["usage"];
            let input = u["input_tokens"].as_i64()?;
            let output = u["output_tokens"].as_i64()?;
            let cache_create = u["cache_creation_input_tokens"].as_i64().unwrap_or(0);
            let cache_read = u["cache_read_input_tokens"].as_i64().unwrap_or(0);
            let total = input + output + cache_create + cache_read;
            Some(ParsedRecord {
                session: event["sessionId"].as_str()?.to_string(),
                response: m["id"].as_str()?.to_string(),
                at,
                at_ns,
                tokens: total,
                complete: true,
                input: Some(input),
                cache_read: Some(cache_read),
                output: Some(output),
            })
        }
        ("antigravity", "PLANNER_RESPONSE") => {
            if event["status"].as_str()? != "DONE" {
                return None;
            }
            let (at, at_ns) = parse_rfc3339(event["created_at"].as_str()?)?;
            let session = session_fallback?.to_string();
            let step = event["step_index"].as_i64().filter(|&n| n >= 0)?;
            let input = optional_nonneg_i64(event, "input_tokens").ok()?;
            let cache_read = optional_nonneg_i64(event, "cache_read_tokens").ok()?;
            let output = optional_nonneg_i64(event, "output_tokens").ok()?;
            // Retain DONE steps even when all three token fields are missing, marked incomplete (unknown).
            let complete = input.is_some() && cache_read.is_some() && output.is_some();
            let tokens = input
                .unwrap_or(0)
                .checked_add(cache_read.unwrap_or(0))?
                .checked_add(output.unwrap_or(0))?;
            Some(ParsedRecord {
                session,
                response: step.to_string(),
                at,
                at_ns,
                tokens,
                complete,
                input,
                cache_read,
                output,
            })
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
        let root = dir.path().join("sessions");
        fs::create_dir_all(&root)?;
        let path = root.join("rollout.jsonl");
        fs::write(&path, "{\"type\":\"session_meta\"}\n")?;
        let mut monitor = Monitor::default();
        let initial = monitor.update(&store, "codex", &root)?;
        assert!(initial.latest.is_none());
        assert_eq!(initial.status, SourceStatus::Active);
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

        // Even without a create event, discover today’s dated directory on the next poll.
        let nested = root.join(today.format("%Y/%m/%d").to_string());
        fs::create_dir_all(&nested)?;
        fs::write(nested.join("new.jsonl"), format!("{}\n", event(4, 1)))?;
        let usage = monitor.update(&store, "codex", &root)?;
        assert_eq!(usage.today, Some(59730));
        assert_eq!(usage.latest, Some(1));
        Ok(())
    }
}
