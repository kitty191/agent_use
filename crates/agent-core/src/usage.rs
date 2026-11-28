use crate::store::Store;
use anyhow::{bail, Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use walkdir::WalkDir;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Active,
    Partial,
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
    pub unreadable_files: usize,
    pub warning: Option<String>,
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
const FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(30);

struct FailedFile {
    sig: Option<(u32, i64, u64)>,
    next_retry: Instant,
    reason: String,
}

#[derive(Default)]
pub struct Monitor {
    watcher: Option<RecommendedWatcher>,
    recent: BTreeSet<(SystemTime, PathBuf)>,
    last_reconcile: Option<Instant>,
    codex_day_dirs: BTreeMap<PathBuf, SystemTime>,
    antigravity_transcripts: BTreeMap<PathBuf, (SystemTime, u64, Option<(u32, i64, u64)>)>,
    known_jsonl_files: BTreeMap<PathBuf, (SystemTime, u64, Option<(u32, i64, u64)>)>,
    failed_files: BTreeMap<PathBuf, FailedFile>,
    repair_next_retry: Option<Instant>,
    changes: Arc<Mutex<Changes>>,
}

#[cfg(unix)]
fn file_perm_sig(meta: &std::fs::Metadata) -> Option<(u32, i64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.mode(), meta.ctime(), meta.len()))
}

#[cfg(not(unix))]
fn file_perm_sig(meta: &std::fs::Metadata) -> Option<(u32, i64, u64)> {
    Some((u32::from(meta.permissions().readonly()), 0, meta.len()))
}

enum OpError {
    Io(anyhow::Error),
    Db(anyhow::Error),
}

impl From<rusqlite::Error> for OpError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Db(err.into())
    }
}

impl Monitor {
    pub fn update(&mut self, store: &Store, id: &str, root: &Path) -> Result<Usage> {
        store.enabled_agent(id)?;
        match std::fs::metadata(root) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.reset_runtime();
                return totals(store, id, SourceStatus::Waiting, 0, None);
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
        let now_inst = Instant::now();
        let discovered_transcripts;
        let unreadable_entries;
        let mut perm_recovered = false;

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
        }
        if id != "antigravity" {
            let (found, unavail) = discover_jsonl_files(root);
            discovered_transcripts = found;
            unreadable_entries = unavail;

            let present_set: BTreeSet<PathBuf> = discovered_transcripts
                .iter()
                .cloned()
                .chain(unreadable_entries.iter().map(|(p, _)| p.clone()))
                .collect();
            self.failed_files.retain(|p, _| present_set.contains(p));

            for (path, reason) in &unreadable_entries {
                self.recent.retain(|(_, p)| p != path);
                let prev = self.failed_files.get(path);
                let next_retry = prev
                    .map(|f| {
                        if now_inst >= f.next_retry {
                            now_inst + FAILED_RETRY_INTERVAL
                        } else {
                            f.next_retry
                        }
                    })
                    .unwrap_or_else(|| now_inst + FAILED_RETRY_INTERVAL);
                self.failed_files.insert(
                    path.clone(),
                    FailedFile {
                        sig: None,
                        next_retry,
                        reason: reason.clone(),
                    },
                );
            }

            let mut current = BTreeMap::new();
            for file_path in &discovered_transcripts {
                match std::fs::metadata(file_path) {
                    Ok(meta) => {
                        let perm_sig = file_perm_sig(&meta);
                        let sig = (meta.modified()?, meta.len(), perm_sig);
                        if let Some(failed) = self.failed_files.get(file_path) {
                            let sig_changed = failed.sig != perm_sig;
                            if sig_changed || now_inst >= failed.next_retry {
                                pending.paths.insert(file_path.clone());
                            }
                        } else if self.known_jsonl_files.get(file_path) != Some(&sig) {
                            pending.paths.insert(file_path.clone());
                        }
                        current.insert(file_path.clone(), sig);
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => {
                        self.recent.retain(|(_, p)| p != file_path);
                        self.failed_files.insert(
                            file_path.clone(),
                            FailedFile {
                                sig: None,
                                next_retry: now_inst + FAILED_RETRY_INTERVAL,
                                reason: format!("无法访问日志文件 {}: {e}", file_path.display()),
                            },
                        );
                    }
                }
            }
            self.known_jsonl_files = current;
        } else {
            let (found, unavail) = discover_antigravity_transcripts(root)?;
            discovered_transcripts = found;
            unreadable_entries = unavail;

            let present_set: BTreeSet<PathBuf> = discovered_transcripts
                .iter()
                .cloned()
                .chain(unreadable_entries.iter().map(|(p, _)| p.clone()))
                .collect();
            self.failed_files.retain(|p, _| present_set.contains(p));

            for (path, reason) in &unreadable_entries {
                self.recent.retain(|(_, p)| p != path);
                let prev = self.failed_files.get(path);
                let next_retry = prev
                    .map(|f| {
                        if now_inst >= f.next_retry {
                            now_inst + FAILED_RETRY_INTERVAL
                        } else {
                            f.next_retry
                        }
                    })
                    .unwrap_or_else(|| now_inst + FAILED_RETRY_INTERVAL);
                self.failed_files.insert(
                    path.clone(),
                    FailedFile {
                        sig: None,
                        next_retry,
                        reason: reason.clone(),
                    },
                );
            }

            let mut current = BTreeMap::new();
            for transcript in &discovered_transcripts {
                match std::fs::metadata(transcript) {
                    Ok(meta) => {
                        let perm_sig = file_perm_sig(&meta);
                        let sig = (meta.modified()?, meta.len(), perm_sig);
                        if let Some(failed) = self.failed_files.get(transcript) {
                            let sig_changed = failed.sig != perm_sig;
                            if sig_changed {
                                perm_recovered = true;
                            }
                            if sig_changed || now_inst >= failed.next_retry {
                                pending.paths.insert(transcript.clone());
                            }
                        } else if self.antigravity_transcripts.get(transcript) != Some(&sig) {
                            pending.paths.insert(transcript.clone());
                        }
                        current.insert(transcript.clone(), sig);
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => {
                        self.recent.retain(|(_, p)| p != transcript);
                        self.failed_files.insert(
                            transcript.clone(),
                            FailedFile {
                                sig: None,
                                next_retry: now_inst + FAILED_RETRY_INTERVAL,
                                reason: format!("无法访问日志文件 {}: {e}", transcript.display()),
                            },
                        );
                    }
                }
            }
            self.antigravity_transcripts = current;
        }

        if id == "antigravity" && store.antigravity_repair_pending()? {
            let due = self.repair_next_retry.is_none_or(|t| now_inst >= t);
            if due || perm_recovered {
                match repair_antigravity_history_inner(store, root) {
                    Ok(()) => self.repair_next_retry = None,
                    Err(OpError::Db(e)) => return Err(e),
                    Err(OpError::Io(e)) => {
                        self.repair_next_retry = Some(now_inst + FAILED_RETRY_INTERVAL);
                        eprintln!("[agent-use] antigravity history repair deferred: {e}");
                    }
                }
            }
        }

        self.sync(store, id, root, pending, reconcile, now_inst)?;

        let present_after: BTreeSet<PathBuf> = discovered_transcripts
            .iter()
            .cloned()
            .chain(unreadable_entries.iter().map(|(p, _)| p.clone()))
            .collect();
        self.failed_files.retain(|p, _| present_after.contains(p));

        let total_seen = discovered_transcripts.len() + unreadable_entries.len();
        if id == "antigravity" && total_seen == 0 {
            return totals(store, id, SourceStatus::Waiting, 0, None);
        }
        let unreadable = self.failed_files.len();
        let readable = total_seen.saturating_sub(unreadable);
        if total_seen > 0 && readable == 0 && unreadable > 0 {
            let first_reason = self
                .failed_files
                .values()
                .next()
                .map(|f| f.reason.as_str())
                .unwrap_or("无读取权限");
            bail!("无法读取任何 {id} 日志文件（共 {unreadable} 份不可读）：{first_reason}");
        }
        if unreadable > 0 {
            let msg = format!("有 {unreadable} 份会话日志不可读，当前统计可能遗漏部分用量");
            return totals(store, id, SourceStatus::Partial, unreadable, Some(msg));
        }

        totals(store, id, SourceStatus::Active, 0, None)
    }

    fn reset_runtime(&mut self) {
        self.watcher = None;
        self.recent.clear();
        self.codex_day_dirs.clear();
        self.antigravity_transcripts.clear();
        self.known_jsonl_files.clear();
        self.failed_files.clear();
        self.repair_next_retry = None;
        self.last_reconcile = None;
    }

    fn sync(
        &mut self,
        store: &Store,
        id: &str,
        root: &Path,
        pending: Changes,
        reconcile: bool,
        now_inst: Instant,
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
            match import(
                &mut db,
                id,
                root,
                &path,
                &mut self.recent,
                &mut self.failed_files,
                now_inst,
            ) {
                Ok(()) => {}
                Err(OpError::Db(error)) => {
                    self.changes.lock().unwrap().all = true;
                    return Err(error);
                }
                Err(OpError::Io(error)) => {
                    self.changes.lock().unwrap().all = true;
                    return Err(error);
                }
            }
        }
        if full {
            self.last_reconcile = Some(now_inst);
        }
        Ok(())
    }
}

pub fn saved_totals(store: &Store, id: &str) -> Result<Usage> {
    totals(store, id, SourceStatus::Waiting, 0, None)
}

fn totals(
    store: &Store,
    id: &str,
    status: SourceStatus,
    unreadable_files: usize,
    warning: Option<String>,
) -> Result<Usage> {
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
        unreadable_files,
        warning,
    })
}

fn discover_jsonl_files(root: &Path) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for entry in WalkDir::new(root) {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_file()
                    && entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl")
                {
                    found.push(entry.into_path());
                }
            }
            Err(e) => {
                let p = e
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf());
                unreadable.push((p, format!("无法访问日志路径: {e}")));
            }
        }
    }
    found.sort();
    unreadable.sort();
    (found, unreadable)
}

fn discover_antigravity_transcripts(root: &Path) -> Result<(Vec<PathBuf>, Vec<(PathBuf, String)>)> {
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("无法读取日志目录 {}", root.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                unreadable.push((root.to_path_buf(), format!("读取目录项失败: {e}")));
                continue;
            }
        };
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
                unreadable.push((
                    transcript.clone(),
                    format!("无法访问日志文件 {}: {e}", transcript.display()),
                ));
            }
        }
    }
    found.sort();
    unreadable.sort();
    Ok((found, unreadable))
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

fn antigravity_conversation_id_from_path(base_root: &Path, path: &Path) -> Option<String> {
    if let Some(id) = antigravity_conversation_id(base_root, path) {
        return Some(id);
    }
    let comps: Vec<_> = path.components().collect();
    if comps.len() < 4 {
        return None;
    }
    let tail = &comps[comps.len() - 4..];
    let [Component::Normal(conv), Component::Normal(sys), Component::Normal(logs), Component::Normal(file)] =
        tail
    else {
        return None;
    };
    if *sys == ".system_generated" && *logs == "logs" && *file == "transcript.jsonl" {
        Some(conv.to_string_lossy().into_owned())
    } else {
        None
    }
}

#[derive(Debug, Clone)]
struct LegacyDbRow {
    at: i64,
    at_ns: Option<i64>,
    tokens: i64,
    complete: i64,
    input: Option<i64>,
    cache_read: Option<i64>,
    output: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReconStep {
    at: i64,
    at_ns: i64,
    tokens: i64,
    complete: bool,
    input: Option<i64>,
    in_ts: Option<i64>,
    cache_read: Option<i64>,
    cr_ts: Option<i64>,
    output: Option<i64>,
    out_ts: Option<i64>,
}

impl ReconStep {
    fn new(rec: &ParsedRecord) -> Self {
        Self {
            at: rec.at,
            at_ns: rec.at_ns,
            tokens: rec.tokens,
            complete: rec.complete,
            input: rec.input,
            in_ts: rec.input.map(|_| rec.at_ns),
            cache_read: rec.cache_read,
            cr_ts: rec.cache_read.map(|_| rec.at_ns),
            output: rec.output,
            out_ts: rec.output.map(|_| rec.at_ns),
        }
    }

    fn merge(&mut self, rec: &ParsedRecord) {
        if rec.at_ns < self.at_ns {
            self.at = rec.at;
            self.at_ns = rec.at_ns;
        }
        let (m_in, m_in_ts) = merge_field(self.input, self.in_ts, rec.input, rec.at_ns);
        let (m_cr, m_cr_ts) = merge_field(self.cache_read, self.cr_ts, rec.cache_read, rec.at_ns);
        let (m_out, m_out_ts) = merge_field(self.output, self.out_ts, rec.output, rec.at_ns);
        self.input = m_in;
        self.in_ts = m_in_ts;
        self.cache_read = m_cr;
        self.cr_ts = m_cr_ts;
        self.output = m_out;
        self.out_ts = m_out_ts;
        self.complete = m_in.is_some() && m_cr.is_some() && m_out.is_some();
        self.tokens = m_in
            .unwrap_or(0)
            .saturating_add(m_cr.unwrap_or(0))
            .saturating_add(m_out.unwrap_or(0));
    }
}

fn load_session_legacy_rows(
    db: &rusqlite::Connection,
    session: &str,
) -> std::result::Result<BTreeMap<String, LegacyDbRow>, OpError> {
    let mut map = BTreeMap::new();
    let mut stmt = db.prepare(
        "SELECT response, at, at_ns, tokens, complete, input_tokens, cache_read_tokens, output_tokens
         FROM usage
         WHERE provider='antigravity' AND session=?1 AND legacy=1",
    )?;
    let rows = stmt.query_map([session], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<i64>>(7)?,
        ))
    })?;
    for row in rows {
        let (resp, at, at_ns, tokens, complete, input, cache_read, output) = row?;
        map.insert(
            resp,
            LegacyDbRow {
                at,
                at_ns,
                tokens,
                complete,
                input,
                cache_read,
                output,
            },
        );
    }
    Ok(map)
}

/// Checks whether a step stored in the legacy DB has independent per-step corroboration in `recon`.
/// Any step where DB lacked full breakdown/timestamp evidence (e.g. `7d76bf1` overwriting `300+20`
/// with partial Dec 1 `100`) returns `false` so uncorroborated fields are never elevated to verified.
fn step_independently_verified(db_row: &LegacyDbRow, recon: &ReconStep, allow_v0: bool) -> bool {
    if let Some(db_ns) = db_row.at_ns {
        recon.at == db_row.at
            && recon.at_ns == db_ns
            && recon.complete == (db_row.complete == 1)
            && recon.tokens == db_row.tokens
            && recon.input == db_row.input
            && recon.cache_read == db_row.cache_read
            && recon.output == db_row.output
    } else if allow_v0
        && db_row.input.is_none()
        && db_row.cache_read.is_none()
        && db_row.output.is_none()
    {
        recon.at == db_row.at && recon.tokens == db_row.tokens
    } else {
        false
    }
}

fn write_verified_recon_step(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    response: &str,
    recon: &ReconStep,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO usage(
            provider,session,response,at,tokens,at_ns,complete,
            input_tokens,cache_read_tokens,output_tokens,legacy,
            input_at_ns,cache_read_at_ns,output_at_ns
         )
         VALUES('antigravity',?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11,?12)",
        params![
            session,
            response,
            recon.at,
            recon.tokens,
            recon.at_ns,
            i64::from(recon.complete),
            recon.input,
            recon.cache_read,
            recon.output,
            recon.in_ts,
            recon.cr_ts,
            recon.out_ts,
        ],
    )?;
    Ok(())
}

/// One-shot resumable history repair for Antigravity, decoupled from `Monitor::update`.
/// Database errors propagate with full context; individual log I/O errors are isolated
/// so remaining sessions finish repairing cleanly while the global marker stays pending.
pub fn repair_antigravity_history(store: &Store, root: &Path) -> Result<()> {
    repair_antigravity_history_inner(store, root).map_err(|e| match e {
        OpError::Db(err) | OpError::Io(err) => err,
    })
}

fn repair_antigravity_history_inner(store: &Store, root: &Path) -> std::result::Result<(), OpError> {
    if !store.antigravity_repair_pending().map_err(OpError::Db)? {
        return Ok(());
    }
    let mut db = store.connect().map_err(|e| OpError::Db(e.into()))?;
    let mut tasks: Vec<(String, String, Option<String>, Option<i64>)> = Vec::new();
    {
        let mut stmt = db.prepare(
            "SELECT kind, target, fingerprint, max_pos
             FROM antigravity_repair_tasks
             WHERE done=0
             ORDER BY kind, target",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        for row in rows {
            tasks.push(row?);
        }
    }

    let mut io_errors = Vec::new();
    for (kind, target, fingerprint, max_pos) in tasks {
        let res = match kind.as_str() {
            "cursor" => repair_cursor_task(
                &mut db,
                root,
                &target,
                fingerprint.as_deref().unwrap_or(""),
                max_pos.unwrap_or(0),
            ),
            "session" => repair_session_task(&mut db, root, &target),
            _ => {
                db.execute(
                    "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
                    [&target],
                )?;
                Ok(())
            }
        };
        match res {
            Ok(()) => {}
            Err(OpError::Db(err)) => return Err(OpError::Db(err)),
            Err(OpError::Io(err)) => io_errors.push(err.to_string()),
        }
    }

    let remaining: i64 = db.query_row(
        "SELECT COUNT(*) FROM antigravity_repair_tasks WHERE done=0",
        [],
        |r| r.get(0),
    )?;
    if remaining == 0 && io_errors.is_empty() {
        store.mark_antigravity_repaired().map_err(OpError::Db)?;
        return Ok(());
    }
    Err(OpError::Io(anyhow::anyhow!(
        "{}",
        if io_errors.is_empty() {
            "仍有未完成的 Antigravity 修复任务".to_string()
        } else {
            io_errors.join("; ")
        }
    )))
}

fn repair_cursor_task(
    db: &mut rusqlite::Connection,
    root: &Path,
    target: &str,
    expected_fp: &str,
    raw_max_pos: i64,
) -> std::result::Result<(), OpError> {
    let max_pos = match u64::try_from(raw_max_pos) {
        Ok(p) => p,
        Err(_) => {
            db.execute(
                "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
                [target],
            )?;
            return Ok(());
        }
    };
    let path = Path::new(target);
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            db.execute(
                "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
                [target],
            )?;
            return Ok(());
        }
        Err(e) => {
            return Err(OpError::Io(
                anyhow::Error::new(e)
                    .context(format!("修复历史时无法访问日志文件 {}", path.display())),
            ));
        }
    };
    if expected_fp.is_empty() || file_id(&meta) != expected_fp || meta.len() < max_pos {
        db.execute(
            "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
            [target],
        )?;
        return Ok(());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            db.execute(
                "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
                [target],
            )?;
            return Ok(());
        }
        Err(e) => {
            return Err(OpError::Io(
                anyhow::Error::new(e)
                    .context(format!("修复历史时无法打开日志文件 {}", path.display())),
            ));
        }
    };
    let opened_fp = file
        .metadata()
        .map_err(|e| OpError::Io(e.into()))?;
    if file_id(&opened_fp) != expected_fp {
        db.execute(
            "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
            [target],
        )?;
        return Ok(());
    }
    let Some(conv_id) = antigravity_conversation_id_from_path(root, path) else {
        db.execute(
            "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
            [target],
        )?;
        return Ok(());
    };

    let legacy_rows = load_session_legacy_rows(db, &conv_id)?;
    if legacy_rows.is_empty() {
        db.execute(
            "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
            [target],
        )?;
        return Ok(());
    }

    let mut reader = BufReader::new(file.take(max_pos));
    let mut recon_map: BTreeMap<String, ReconStep> = BTreeMap::new();
    let mut bytes = Vec::new();
    let mut read_pos = 0u64;
    loop {
        bytes.clear();
        let len = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| OpError::Io(e.into()))?;
        if len == 0 || bytes.last() != Some(&b'\n') {
            break;
        }
        read_pos += len as u64;
        if let Ok(event) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(rec) = record("antigravity", Some(&conv_id), &event) {
                recon_map
                    .entry(rec.response.clone())
                    .and_modify(|s| s.merge(&rec))
                    .or_insert_with(|| ReconStep::new(&rec));
            }
        }
    }

    let mut verified_steps = Vec::new();
    for (resp, db_row) in &legacy_rows {
        if let Some(recon) = recon_map.get(resp) {
            if step_independently_verified(db_row, recon, true) {
                verified_steps.push((resp.as_str(), recon));
            }
        }
    }

    let tx = db.transaction()?;
    for (resp, recon) in verified_steps {
        write_verified_recon_step(&tx, &conv_id, resp, recon)?;
    }
    // Hand off the repaired prefix boundary so subsequent incremental polls never
    // re-read [0..read_pos) or promote unverified legacy=1 steps on re-enable.
    let pos_i64 = i64::try_from(read_pos).map_err(|e| OpError::Io(e.into()))?;
    tx.execute(
        "INSERT OR REPLACE INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
        params![target, expected_fp, pos_i64],
    )?;
    tx.execute(
        "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
        [target],
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Default)]
struct StepMatchTracker {
    matched: Option<(u64, ReconStep)>,
    diverged_after_match: bool,
    ambiguous: bool,
}

impl StepMatchTracker {
    fn observe(&mut self, byte_pos: u64, db_row: &LegacyDbRow, recon: &ReconStep) {
        if self.ambiguous {
            return;
        }
        let is_match = step_independently_verified(db_row, recon, false);
        match (&self.matched, is_match) {
            (None, true) => {
                self.matched = Some((byte_pos, recon.clone()));
            }
            (Some(_), false) if !self.diverged_after_match => {
                self.diverged_after_match = true;
            }
            (Some(_), true) if self.diverged_after_match => {
                self.ambiguous = true;
            }
            _ => {}
        }
    }
}

fn repair_session_task(
    db: &mut rusqlite::Connection,
    root: &Path,
    session: &str,
) -> std::result::Result<(), OpError> {
    let legacy_rows = load_session_legacy_rows(db, session)?;
    if legacy_rows.is_empty() {
        db.execute(
            "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
            [session],
        )?;
        return Ok(());
    }

    let path = root
        .join(session)
        .join(".system_generated/logs/transcript.jsonl");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            db.execute(
                "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
                [session],
            )?;
            return Ok(());
        }
        Err(e) => {
            return Err(OpError::Io(
                anyhow::Error::new(e)
                    .context(format!("修复历史时无法打开日志文件 {}", path.display())),
            ));
        }
    };
    let fp = file_id(&file.metadata().map_err(|e| OpError::Io(e.into()))?);
    let mut reader = BufReader::new(file);
    let mut recon_map: BTreeMap<String, ReconStep> = BTreeMap::new();
    let mut trackers: BTreeMap<String, StepMatchTracker> = legacy_rows
        .keys()
        .map(|k| (k.clone(), StepMatchTracker::default()))
        .collect();
    let mut bytes = Vec::new();
    let mut byte_pos = 0u64;
    let mut max_legacy_step_pos = 0u64;
    let mut first_untracked_step_pos: Option<u64> = None;

    loop {
        bytes.clear();
        let line_start = byte_pos;
        let len = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| OpError::Io(e.into()))?;
        if len == 0 || bytes.last() != Some(&b'\n') {
            break;
        }
        byte_pos += len as u64;
        if let Ok(event) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(rec) = record("antigravity", Some(session), &event) {
                let resp = rec.response.clone();
                if legacy_rows.contains_key(&resp) {
                    max_legacy_step_pos = max_legacy_step_pos.max(byte_pos);
                } else {
                    first_untracked_step_pos.get_or_insert(line_start);
                }
                let recon = recon_map
                    .entry(resp.clone())
                    .and_modify(|s| s.merge(&rec))
                    .or_insert_with(|| ReconStep::new(&rec));
                if let (Some(db_row), Some(tracker)) =
                    (legacy_rows.get(&resp), trackers.get_mut(&resp))
                {
                    tracker.observe(byte_pos, db_row, recon);
                }
            }
        }
    }

    let mut verified_steps = Vec::new();
    let mut max_verified_pos = 0u64;
    for (resp, tracker) in &trackers {
        if !tracker.ambiguous {
            if let Some((pos, recon)) = &tracker.matched {
                max_verified_pos = max_verified_pos.max(*pos);
                verified_steps.push((resp.as_str(), recon));
            }
        }
    }
    let all_verified = verified_steps.len() == legacy_rows.len();
    // Anchor the file cursor without jumping past any brand-new step (e.g. Step 3)
    // written before a late backfill of an older step (e.g. Step 2).
    let cursor_pos = if max_verified_pos == 0 && max_legacy_step_pos == 0 {
        byte_pos
    } else {
        let raw = if all_verified {
            max_verified_pos
        } else {
            max_verified_pos.max(max_legacy_step_pos)
        };
        first_untracked_step_pos.map_or(raw, |first| raw.min(first))
    };

    let tx = db.transaction()?;
    for (resp, recon) in verified_steps {
        write_verified_recon_step(&tx, session, resp, recon)?;
    }
    let pos_i64 = i64::try_from(cursor_pos).map_err(|e| OpError::Io(e.into()))?;
    tx.execute(
        "INSERT OR REPLACE INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
        params![path.to_string_lossy(), fp, pos_i64],
    )?;
    tx.execute(
        "UPDATE antigravity_repair_tasks SET done=1 WHERE target=?1",
        [session],
    )?;
    tx.commit()?;
    Ok(())
}

fn antigravity_file_pending_repair(
    db: &rusqlite::Connection,
    file_path: &Path,
    conv_id: Option<&str>,
) -> Result<bool> {
    let key = file_path.to_string_lossy();
    let by_path: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM antigravity_repair_tasks WHERE kind='cursor' AND target=?1 AND done=0)",
        [key.as_ref()],
        |r| r.get(0),
    )?;
    if by_path {
        return Ok(true);
    }
    if let Some(session) = conv_id {
        let by_session: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM antigravity_repair_tasks WHERE kind='session' AND target=?1 AND done=0)",
            [session],
            |r| r.get(0),
        )?;
        if by_session {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_under_root(base_root: &Path, path: &Path) -> PathBuf {
    if path.starts_with(base_root) {
        return path.to_path_buf();
    }
    if let (Ok(canon_root), Ok(canon_path)) = (base_root.canonicalize(), path.canonicalize()) {
        if let Ok(rel) = canon_path.strip_prefix(&canon_root) {
            return base_root.join(rel);
        }
    }
    path.to_path_buf()
}

fn import(
    db: &mut rusqlite::Connection,
    id: &str,
    base_root: &Path,
    target: &Path,
    recent: &mut BTreeSet<(SystemTime, PathBuf)>,
    failed_antigravity: &mut BTreeMap<PathBuf, FailedFile>,
    now_inst: Instant,
) -> std::result::Result<(), OpError> {
    let normalized_target = normalize_under_root(base_root, target);
    let target = normalized_target.as_path();
    if !target.exists() {
        return Ok(());
    }
    if id == "antigravity" {
        let files = if target == base_root {
            let (found, unavail) = discover_antigravity_transcripts(base_root).map_err(OpError::Io)?;
            for (path, reason) in unavail {
                recent.retain(|(_, p)| p != &path);
                let prev = failed_antigravity.get(&path);
                let next_retry = prev
                    .map(|f| {
                        if now_inst >= f.next_retry {
                            now_inst + FAILED_RETRY_INTERVAL
                        } else {
                            f.next_retry
                        }
                    })
                    .unwrap_or_else(|| now_inst + FAILED_RETRY_INTERVAL);
                failed_antigravity.insert(
                    path,
                    FailedFile {
                        sig: None,
                        next_retry,
                        reason,
                    },
                );
            }
            found
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
            if let Some(failed) = failed_antigravity.get(&file_path) {
                let current_sig = std::fs::metadata(&file_path)
                    .ok()
                    .and_then(|m| file_perm_sig(&m));
                if current_sig == failed.sig && now_inst < failed.next_retry {
                    continue;
                }
            }
            if antigravity_file_pending_repair(db, &file_path, conv_id.as_deref())
                .map_err(OpError::Db)?
            {
                recent.retain(|(_, p)| p != &file_path);
                let current_sig = std::fs::metadata(&file_path)
                    .ok()
                    .and_then(|m| file_perm_sig(&m));
                failed_antigravity.insert(
                    file_path,
                    FailedFile {
                        sig: current_sig,
                        next_retry: now_inst + FAILED_RETRY_INTERVAL,
                        reason: "等待历史修复完成".to_string(),
                    },
                );
                continue;
            }
            match import_file(db, id, &file_path, conv_id.as_deref(), recent) {
                Ok(()) => {
                    failed_antigravity.remove(&file_path);
                }
                Err(OpError::Db(err)) => return Err(OpError::Db(err)),
                Err(OpError::Io(err)) => {
                    recent.retain(|(_, p)| p != &file_path);
                    let sig = std::fs::metadata(&file_path)
                        .ok()
                        .and_then(|m| file_perm_sig(&m));
                    failed_antigravity.insert(
                        file_path,
                        FailedFile {
                            sig,
                            next_retry: now_inst + FAILED_RETRY_INTERVAL,
                            reason: err.to_string(),
                        },
                    );
                }
            }
        }
        return Ok(());
    }

    for entry in WalkDir::new(target) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let p = e
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| target.to_path_buf());
                recent.retain(|(_, rp)| rp != &p);
                failed_antigravity.insert(
                    p,
                    FailedFile {
                        sig: None,
                        next_retry: now_inst + FAILED_RETRY_INTERVAL,
                        reason: format!("无法访问日志路径: {e}"),
                    },
                );
                continue;
            }
        };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            continue;
        }
        let file_path = entry.into_path();
        if let Some(failed) = failed_antigravity.get(&file_path) {
            let current_sig = std::fs::metadata(&file_path)
                .ok()
                .and_then(|m| file_perm_sig(&m));
            if current_sig == failed.sig && now_inst < failed.next_retry {
                continue;
            }
        }
        match import_file(db, id, &file_path, None, recent) {
            Ok(()) => {
                failed_antigravity.remove(&file_path);
            }
            Err(OpError::Db(err)) => return Err(OpError::Db(err)),
            Err(OpError::Io(err)) => {
                recent.retain(|(_, p)| p != &file_path);
                let sig = std::fs::metadata(&file_path)
                    .ok()
                    .and_then(|m| file_perm_sig(&m));
                failed_antigravity.insert(
                    file_path,
                    FailedFile {
                        sig,
                        next_retry: now_inst + FAILED_RETRY_INTERVAL,
                        reason: err.to_string(),
                    },
                );
            }
        }
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
        Some((_, _, _, _, _, 1, _, _, _)) => {
            // Preserve rows intentionally left as unverified legacy=1 by history repair.
            return Ok(());
        }
        None => {
            // New verified row initialized from on-disk transcript.
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
) -> std::result::Result<(), OpError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(OpError::Io(
                anyhow::Error::new(e).context(format!("无法读取日志文件 {}", path.display())),
            ));
        }
    };
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
    // Open the file before checking EOF so permission revocation is detected immediately
    // even when cursor already reached the end of file, while still reading 0 bytes at EOF.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(OpError::Io(
                anyhow::Error::new(e).context(format!("无法打开日志文件 {}", path.display())),
            ));
        }
    };
    let opened_meta = file.metadata().map_err(|e| OpError::Io(e.into()))?;
    if file_id(&opened_meta) != fingerprint {
        return Ok(());
    }
    let modified = opened_meta.modified().map_err(|e| OpError::Io(e.into()))?;
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
    if pos == opened_meta.len() {
        return Ok(());
    }
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(pos))
        .map_err(|e| OpError::Io(e.into()))?;
    let tx = db.transaction()?;
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        let len = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| OpError::Io(e.into()))?;
        if len == 0 || bytes.last() != Some(&b'\n') {
            break;
        }
        pos += len as u64;
        if let Ok(event) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(rec) = record(id, session_fallback, &event) {
                if id == "antigravity" {
                    upsert_antigravity_record(&tx, &rec).map_err(OpError::Db)?;
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
    let pos_i64 = i64::try_from(pos).map_err(|e| OpError::Io(e.into()))?;
    tx.execute(
        "INSERT OR REPLACE INTO cursors VALUES(?1,?2,?3,?4)",
        params![id, key, fingerprint, pos_i64],
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

    #[cfg(unix)]
    #[test]
    fn antigravity_persistent_permission_error_throttles_full_rescans() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("throttle.sqlite"))?;
        store.settings(false, || {
            vec![Agent {
                id: "antigravity".into(),
                name: "Antigravity".into(),
                binary: None,
                status: "installed".into(),
            }]
        })?;
        store.select(vec!["antigravity".into()])?;

        let brain = dir.path().join("brain");
        let la = brain.join("conv-a/.system_generated/logs");
        let lz = brain.join("conv-z/.system_generated/logs");
        fs::create_dir_all(&la)?;
        fs::create_dir_all(&lz)?;
        let ta = la.join("transcript.jsonl");
        let tz = lz.join("transcript.jsonl");
        let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
        fs::write(
            &ta,
            format!(
                "{}\n",
                json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": format!("{today}T09:00:00+08:00"), "input_tokens": 50, "cache_read_tokens": 25, "output_tokens": 25})
            ),
        )?;
        fs::write(
            &tz,
            format!(
                "{}\n",
                json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": format!("{today}T10:00:00+08:00"), "input_tokens": 200, "cache_read_tokens": 12, "output_tokens": 10})
            ),
        )?;
        let orig = fs::metadata(&ta)?.permissions();
        fs::set_permissions(&ta, fs::Permissions::from_mode(0o000))?;

        let mut mon = Monitor::default();
        let u1 = mon.update(&store, "antigravity", &brain)?;
        assert_eq!(u1.status, SourceStatus::Partial);
        assert_eq!(u1.unreadable_files, 1);
        let first_reconcile = mon.last_reconcile.expect("reconcile stamp should stay set");
        assert!(!mon.changes.lock().unwrap().all, "must not force full rescan on every poll");
        assert!(mon.failed_files.contains_key(&ta));

        // Second poll within retry window reuses cached failed entry without full rescan or opening ta.
        let u2 = mon.update(&store, "antigravity", &brain)?;
        assert_eq!(u2.status, SourceStatus::Partial);
        assert_eq!(mon.last_reconcile, Some(first_reconcile));
        assert!(!mon.changes.lock().unwrap().all);

        // Restoring permission updates perm signature -> immediate catch-up on very next poll!
        fs::set_permissions(&ta, orig)?;
        let u3 = mon.update(&store, "antigravity", &brain)?;
        assert_eq!(u3.status, SourceStatus::Active);
        assert_eq!(u3.today, Some(322));
        assert!(mon.failed_files.is_empty());
        Ok(())
    }
}
