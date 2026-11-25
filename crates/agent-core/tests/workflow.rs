use agent_core::{discovery::Agent, quota, store::Store, usage::Monitor};
use serde_json::json;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn agent(id: &str, status: &str) -> Agent {
    Agent {
        id: id.into(),
        name: id.into(),
        status: status.into(),
        binary: None,
    }
}

#[test]
fn empty_scan_restart_rescan_and_opt_in() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("state.db");
    let store = Store::open(&path)?;
    assert!(store.settings(false, Vec::new)?.agents.is_empty());
    let reopened = Store::open(&path)?;
    reopened.settings(false, || panic!("second launch scanned again"))?;
    reopened.settings(true, || vec![agent("codex", "installed")])?;
    assert!(reopened.enabled_agent("codex").is_err());
    reopened.select(vec!["codex".into()])?;
    let s = reopened.settings(true, || {
        vec![agent("codex", "missing"), agent("claude", "data")]
    })?;
    assert_eq!(s.enabled, ["codex"]);
    assert!(s.confirmed);
    assert!(reopened.enabled_agent("claude").is_err());
    reopened.select(vec![])?;
    assert!(reopened.cached_quota().is_err());
    Ok(())
}

#[test]
fn watcher_handles_new_file_partial_line_replay_and_restart() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("state.db"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;
    let root = dir.path().join("sessions");
    fs::create_dir_all(&root)?;
    let mut mon = Monitor::default();
    assert_eq!(mon.update(&store, "codex", &root)?.today, None);
    let path = root.join("new.jsonl");
    let event=json!({"type":"token_usage_record","timestamp":chrono::Utc::now().to_rfc3339(),"payload":{"session_id":"s1","response_id":"r1","usage":{"total_tokens":27,"cached_input_tokens":5}}}).to_string();
    fs::write(&path, &event)?;
    assert_eq!(mon.update(&store, "codex", &root)?.latest, None);
    let mut f = fs::OpenOptions::new().append(true).open(&path)?;
    writeln!(f)?;
    writeln!(f, "{event}")?;
    writeln!(
        f,
        "{}",
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":27}}}})
    )?;
    let start = Instant::now();
    while mon.update(&store, "codex", &root)?.today != Some(27) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(5), "new usage missed");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(mon);
    assert_eq!(
        Monitor::default().update(&store, "codex", &root)?.today,
        Some(27)
    );
    Ok(())
}

#[test]
fn claude_updates_message_and_includes_cache() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("usage.db"))?;
    store.settings(false, || vec![agent("claude", "data")])?;
    store.select(vec!["claude".into()])?;
    let at = chrono::Utc::now().to_rfc3339();
    let rows=[1,8].map(|out|json!({"type":"assistant","timestamp":at,"sessionId":"s","message":{"id":"m","usage":{"input_tokens":2,"output_tokens":out,"cache_read_input_tokens":5,"cache_creation_input_tokens":1}}}).to_string());
    fs::write(dir.path().join("claude.jsonl"), rows.join("\n") + "\n")?;
    assert_eq!(
        Monitor::default()
            .update(&store, "claude", dir.path())?
            .today,
        Some(16)
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn quota_reads_only_status_methods_and_returns_one_real_window() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;
    let bin = dir.path().join("codex");
    fs::write(
        &bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized':continue
    if msg['method']=='initialize':result={}
    elif msg['method']=='account/read':
        assert msg['params']=={'refreshToken':False}
        result={'account':{'type':'chatgpt','email':'test@example.org','planType':'plus'}}
    elif msg['method']=='account/rateLimits/read':
        result={'rateLimitsByLimitId':{'codex':{'primary':{'windowDurationMins':10080,'usedPercent':16,'resetsAt':1795935437},'secondary':None}}}
    else: sys.exit(1)
    print(json.dumps({'id':msg['id'],'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
    let q = quota::query(&bin).await?;
    assert_eq!(q.account, "t***@example.org");
    assert_eq!(q.windows.len(), 1);
    assert_eq!(q.windows[0].minutes, Some(10080));
    assert_eq!(100.0 - q.windows[0].used, 84.0);
    Ok(())
}

#[test]
fn codex_live_rollout_in_new_date_directory_counts_each_response_once() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir(&sessions)?;
    let mut mon = Monitor::default();
    assert_eq!(mon.update(&store, "codex", &sessions)?.today, None);
    let date_dir = sessions.join(chrono::Local::now().format("%Y/%m/%d").to_string());
    fs::create_dir_all(&date_dir)?;
    let rollout = date_dir.join("rollout-example.jsonl");
    let mut writer = fs::File::create(&rollout)?;
    writeln!(
        writer,
        "{}",
        json!({"type":"session_meta","payload":{"id":"sample-session"}})
    )?;
    let mut total = 0;
    for (n, tokens) in [19895, 19908, 19926].into_iter().enumerate() {
        let timestamp = chrono::Utc::now().to_rfc3339();
        let counters = json!({"input_tokens":tokens-5,"cached_input_tokens":0,"output_tokens":5,
                              "reasoning_output_tokens":0,"total_tokens":tokens});
        total += tokens;
        let response = format!("sample-response-{n}");
        let record = json!({"type":"token_usage_record","timestamp":timestamp,"payload":{
            "session_id":"sample-session","thread_id":"sample-thread","response_id":response,
            "turn_id":format!("turn-{n}"),"root_turn_id":format!("turn-{n}"),
            "usage":counters,"turn_token_usage":counters,"thread_token_usage":{"total_tokens":total}
        }});
        let summary = json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"token_count",
            "info":{"last_token_usage":counters,"total_token_usage":{"total_tokens":total},
                    "model_context_window":128000},"rate_limits":null}});
        // While a JSONL line is incomplete, the cursor must stop before the line.
        write!(writer, "{record}")?;
        writer.flush()?;
        assert_eq!(
            mon.update(&store, "codex", &sessions)?.today,
            (n > 0).then_some(total - tokens)
        );
        writeln!(writer)?;
        writeln!(writer, "{summary}")?;
        writeln!(writer, "{record}")?;
        writer.flush()?;
        let start = Instant::now();
        loop {
            let state = mon.update(&store, "codex", &sessions)?;
            anyhow::ensure!(
                start.elapsed() < Duration::from_secs(2),
                "complete line missed the two-second backend budget"
            );
            if state.today == Some(total) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let db = store.connect()?;
        let (rows,stored):(i64,i64)=db.query_row(
            "SELECT COUNT(*), SUM(tokens) FROM usage WHERE provider='codex' AND session='sample-session' AND response=?1",
            [&response],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!((rows, stored), (1, tokens));
        let cursor: i64 = db.query_row(
            "SELECT pos FROM cursors WHERE provider='codex' AND path=?1",
            [rollout.to_str().unwrap()],
            |r| r.get(0),
        )?;
        assert_eq!(cursor as u64, writer.metadata()?.len());
    }
    assert_eq!(total, 59729);
    assert_eq!(
        Monitor::default().update(&store, "codex", &sessions)?.today,
        Some(59729)
    );
    Ok(())
}
