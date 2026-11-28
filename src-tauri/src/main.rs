#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use agent_core::{
    discovery, history, quota,
    store::{Settings, Store},
    usage,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Manager, State};

struct App {
    db: Arc<Store>,
    home: PathBuf,
    data_dir: PathBuf,
    querying: Mutex<HashSet<String>>,
    monitors: Mutex<HashMap<String, Arc<Mutex<usage::Monitor>>>>,
}

#[tauri::command]
fn load_settings(app: State<App>, rescan: bool) -> Result<Settings, String> {
    app.db
        .settings(rescan, || discovery::scan(&app.home))
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn save_selection(app: State<App>, ids: Vec<String>) -> Result<Settings, String> {
    let mut monitors = app.monitors.lock().map_err(|e| e.to_string())?;
    let state = app.db.select(ids).map_err(|e| e.to_string())?;
    monitors.retain(|id, _| state.enabled.contains(id));
    Ok(state)
}
#[tauri::command]
fn antigravity_bridge_info(
    handle: tauri::AppHandle,
    app: State<App>,
) -> Result<quota::BridgeInfo, String> {
    let bundled = handle
        .path()
        .resource_dir()
        .ok()
        .map(|d| d.join("antigravity-statusline-bridge.sh"))
        .filter(|p| p.is_file());
    let script_path = bundled
        .or_else(|| {
            let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../scripts/antigravity-statusline-bridge.sh");
            dev.canonicalize().ok().filter(|p| p.is_file())
        })
        .unwrap_or_else(|| {
            PathBuf::from(
                "/Applications/Agent Use.app/Contents/Resources/antigravity-statusline-bridge.sh",
            )
        });
    let snapshot_path = app.data_dir.join("antigravity_statusline.json");
    quota::build_antigravity_bridge_info(&script_path, &snapshot_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn last_quota(app: State<App>) -> Result<Option<quota::Quota>, String> {
    app.db.cached_quota().map_err(|e| e.to_string())
}

#[tauri::command]
fn last_provider_quota(app: State<App>, id: String) -> Result<Option<quota::Quota>, String> {
    app.db.cached_quota_for(&id, None).map_err(|e| e.to_string())
}

#[tauri::command]
fn saved_usage(app: State<App>, id: String) -> Result<usage::Usage, String> {
    usage::saved_totals(&app.db, &id).map_err(|e| e.to_string())
}

struct ProviderQueryGuard<'a>(&'a Mutex<HashSet<String>>, String);
impl Drop for ProviderQueryGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.0.lock() {
            set.remove(&self.1);
        }
    }
}

#[tauri::command]
async fn refresh_quota(app: State<'_, App>) -> Result<quota::Quota, String> {
    let res = refresh_provider_quota_inner(&app, "codex").await?;
    res.quota.ok_or_else(|| "暂无已确认的 Codex 额度数据".into())
}

#[tauri::command]
async fn refresh_provider_quota(
    app: State<'_, App>,
    id: String,
) -> Result<quota::QuotaRefreshResult, String> {
    refresh_provider_quota_inner(&app, &id).await
}

async fn refresh_provider_quota_inner(
    app: &App,
    id: &str,
) -> Result<quota::QuotaRefreshResult, String> {
    {
        let mut active = app.querying.lock().map_err(|e| e.to_string())?;
        if !active.insert(id.to_string()) {
            return Err("正在查询该平台额度".into());
        }
    }
    let _guard = ProviderQueryGuard(&app.querying, id.to_string());

    if id == "codex" {
        let codex = app.db.enabled_agent("codex").map_err(|e| e.to_string())?;
        let binary = codex
            .binary
            .ok_or("未找到 Codex CLI，安装后请点击「重新扫描」")?;
        if !binary.is_file() {
            return Err("已记录的 Codex CLI 路径不可用，请点击「重新扫描」重新定位".into());
        }
        let snapshot = quota::query(&binary).await.map_err(|e| e.to_string())?;
        let (effective, updated, note) = app
            .db
            .merge_quota_for("codex", &snapshot)
            .map_err(|e| e.to_string())?;
        return Ok(quota::QuotaRefreshResult {
            quota: effective,
            fresh: updated,
            message: note,
        });
    }

    if id == "antigravity" {
        app.db
            .enabled_agent("antigravity")
            .map_err(|e| e.to_string())?;
        if let Some(path) = quota::discover_antigravity_statusline_path(&app.data_dir, &app.home) {
            let snapshot = quota::read_antigravity_statusline(&path).map_err(|e| e.to_string())?;
            let (effective, _updated, note) = app
                .db
                .merge_quota_for("antigravity", &snapshot)
                .map_err(|e| e.to_string())?;
            return Ok(quota::QuotaRefreshResult {
                quota: effective,
                // Local statusLine file import is a captured snapshot, never an online live query.
                fresh: false,
                message: note,
            });
        }
        let cached = app
            .db
            .cached_quota_for("antigravity", None)
            .map_err(|e| e.to_string())?;
        let msg = if cached.is_some() {
            "未检测到新的 Antigravity CLI statusLine 快照，保留上次采集结果；桌面版可在 Antigravity 设置 → Models 查看"
        } else {
            "当前仅检测到 Antigravity 桌面版，模型限额请在 Antigravity 设置 → Models 查看；若使用 Antigravity CLI，可复制下方桥接配置接入 statusLine 快照。"
        };
        return Ok(quota::QuotaRefreshResult {
            quota: cached,
            fresh: false,
            message: Some(msg.into()),
        });
    }

    app.db.enabled_agent(id).map_err(|e| e.to_string())?;
    let cached = app
        .db
        .cached_quota_for(id, None)
        .map_err(|e| e.to_string())?;
    Ok(quota::QuotaRefreshResult {
        quota: cached,
        fresh: false,
        message: Some("当前平台暂无可自动读取的在线额度来源".into()),
    })
}
#[tauri::command]
async fn local_usage(app: State<'_, App>, id: String) -> Result<usage::Usage, String> {
    let monitor = {
        let mut monitors = app.monitors.lock().map_err(|e| e.to_string())?;
        app.db.enabled_agent(&id).map_err(|e| e.to_string())?;
        monitors.entry(id.clone()).or_default().clone()
    };
    let db = Arc::clone(&app.db);
    let root = discovery::log_root(&id, &app.home).ok_or("缺少用量来源")?;
    tauri::async_runtime::spawn_blocking(move || {
        monitor
            .lock()
            .map_err(|e| e.to_string())?
            .update(&db, &id, &root)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn usage_history(
    app: State<App>,
    granularity: String,
    offset: u32,
    provider: Option<String>,
    range_start: Option<String>,
    range_end: Option<String>,
    drill_key: Option<String>,
) -> Result<history::HistoryPage, String> {
    let gran = history::Granularity::parse(&granularity).map_err(|e| e.to_string())?;
    history::query_with_drill(
        &app.db,
        gran,
        offset,
        provider.as_deref(),
        range_start.as_deref(),
        range_end.as_deref(),
        drill_key.as_deref(),
    )
    .map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let path = std::env::var_os("AGENT_USE_DATA_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or(app.path().app_local_data_dir()?);
            std::fs::create_dir_all(&path)?;
            let db = Store::open(&path.join("monitor.sqlite"))?;
            let home = app.path().home_dir()?;
            if let Some(root) = discovery::log_root("antigravity", &home) {
                if let Err(err) = usage::repair_antigravity_history(&db, &root) {
                    eprintln!("[agent-use] antigravity history repair deferred: {err}");
                }
            }
            app.manage(App {
                db: Arc::new(db),
                home,
                data_dir: path,
                querying: Mutex::default(),
                monitors: Mutex::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_selection,
            antigravity_bridge_info,
            last_quota,
            refresh_quota,
            last_provider_quota,
            refresh_provider_quota,
            saved_usage,
            local_usage,
            usage_history
        ])
        .run(tauri::generate_context!())
        .expect("启动 Agent Use 失败");
}
