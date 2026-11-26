#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use agent_core::{
    discovery, history, quota,
    store::{Settings, Store},
    usage,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Manager, State};

struct App {
    db: Arc<Store>,
    home: PathBuf,
    querying: std::sync::atomic::AtomicBool,
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
fn last_quota(app: State<App>) -> Result<Option<quota::Quota>, String> {
    app.db.cached_quota().map_err(|e| e.to_string())
}
struct QueryGuard<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for QueryGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[tauri::command]
async fn refresh_quota(app: State<'_, App>) -> Result<quota::Quota, String> {
    use std::sync::atomic::Ordering;
    let codex = app.db.enabled_agent("codex").map_err(|e| e.to_string())?;
    let binary = codex.binary.ok_or("未找到 Codex CLI，安装后请点击「重新扫描」")?;
    if !binary.is_file() {
        return Err("已记录的 Codex CLI 路径不可用，请点击「重新扫描」重新定位".into());
    }
    if app.querying.swap(true, Ordering::AcqRel) {
        return Err("正在查询 Codex".into());
    }
    let _guard = QueryGuard(&app.querying);
    let snapshot = quota::query(&binary).await.map_err(|e| e.to_string())?;
    app.db.save_quota(&snapshot).map_err(|e| e.to_string())?;
    Ok(snapshot)
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
                querying: false.into(),
                monitors: Mutex::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_selection,
            last_quota,
            refresh_quota,
            local_usage,
            usage_history
        ])
        .run(tauri::generate_context!())
        .expect("启动 Agent Use 失败");
}
