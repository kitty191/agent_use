use agent_core::{
    discovery::Agent,
    history::{self, Granularity},
    quota,
    store::Store,
    usage::{self, Monitor, SourceStatus},
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn test_file_id(path: &std::path::Path) -> anyhow::Result<String> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("{:?}", meta.created().ok()))
    }
}

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

#[cfg(unix)]
#[tokio::test]
async fn quota_tolerates_null_error_and_distinguishes_failure_stages() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;

    // 1. CLI missing
    let missing = dir.path().join("missing-codex");
    let err = quota::query(&missing).await.unwrap_err().to_string();
    assert!(err.contains("未找到可执行的 Codex CLI"), "got: {err}");

    // 2. Tolerates non-JSON stdout lines and explicit `"error": null`
    let ok_bin = dir.path().join("codex-ok");
    fs::write(
        &ok_bin,
        r#"#!/usr/bin/env python3
import json,sys
print("startup diagnostic banner", flush=True)
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize': result={}
    elif msg['method']=='account/read':
        result={'account':{'type':'chatgpt','email':'alice@example.org','planType':'plus'}}
    elif msg['method']=='account/rateLimits/read':
        result={'rateLimitsByLimitId':{'codex':{'primary':{'windowDurationMins':10080,'usedPercent':20,'resetsAt':1795935437},'secondary':None}}}
    print(json.dumps({'id':msg['id'],'error':None,'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&ok_bin, fs::Permissions::from_mode(0o700))?;
    let q = quota::query(&ok_bin).await?;
    assert_eq!(q.account, "a***@example.org");
    assert_eq!(q.windows.len(), 1);

    // 3. Not logged in
    let unauth_bin = dir.path().join("codex-unauth");
    fs::write(
        &unauth_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    result={} if msg['method']=='initialize' else {'account':None}
    print(json.dumps({'id':msg['id'],'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&unauth_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unauth_bin).await.unwrap_err().to_string();
    assert!(err.contains("尚未登录"), "got: {err}");

    // 4. Network error during rateLimits/read
    let net_bin = dir.path().join("codex-net");
    fs::write(
        &net_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32000,'message':'dns lookup failed'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&net_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&net_bin).await.unwrap_err().to_string();
    assert!(err.contains("网络查询失败"), "got: {err}");

    // 5. Rate limit (429)
    let rl_bin = dir.path().join("codex-429");
    fs::write(
        &rl_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':429,'message':'HTTP 429 Too Many Requests'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&rl_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&rl_bin).await.unwrap_err().to_string();
    assert!(err.contains("限流"), "got: {err}");

    // 6. Method not supported (e.g., older Codex CLI) must not blame network
    let unsupported_bin = dir.path().join("codex-unsupported");
    fs::write(
        &unsupported_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32601,'message':'Method not found'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&unsupported_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unsupported_bin)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("不支持"), "got: {err}");
    assert!(!err.contains("网络"), "got: {err}");

    // 7. Unknown RPC error on rateLimits/read must not blame network either
    let unknown_bin = dir.path().join("codex-unknown");
    fs::write(
        &unknown_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32099,'message':'unexpected internal state'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&unknown_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unknown_bin).await.unwrap_err().to_string();
    assert!(err.contains("未知错误"), "got: {err}");
    assert!(!err.contains("网络"), "got: {err}");

    // 8. Overall timeout when CLI hangs on initialize must not blame network
    let hang_bin = dir.path().join("codex-hang");
    fs::write(
        &hang_bin,
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.readline()
time.sleep(30)
"#,
    )?;
    fs::set_permissions(&hang_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query_with_timeout(&hang_bin, Duration::from_millis(250))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "Codex 查询超时，请稍后重试");
    assert!(!err.contains("网络"), "got: {err}");

    Ok(())
}

#[test]
fn antigravity_hidden_transcript_dedup_partial_line_restart_and_waiting_recovery()
-> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || {
        vec![
            agent("codex", "installed"),
            agent("antigravity", "installed"),
        ]
    })?;

    // Only Codex enabled: Antigravity must not be readable.
    store.select(vec!["codex".into()])?;
    let brain = dir.path().join("brain");
    assert!(Monitor::default().update(&store, "antigravity", &brain).is_err());

    // Enable Antigravity before brain directory exists -> Waiting (not Active, not error).
    store.select(vec!["codex".into(), "antigravity".into()])?;
    let mut mon = Monitor::default();
    let waiting = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(waiting.status, SourceStatus::Waiting);
    assert_eq!(waiting.today, None);
    assert_eq!(waiting.latest, None);

    // Brain directory appears without logs yet -> stays Waiting; empty state has None (not fake 0).
    fs::create_dir_all(&brain)?;
    let empty_waiting = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(empty_waiting.status, SourceStatus::Waiting);
    assert_eq!(empty_waiting.today, None);

    // Create a new conversation under hidden `.system_generated/logs/transcript.jsonl`,
    // plus duplicate `transcript_full.jsonl` and `chunks/transcript/00000000.jsonl` which must be ignored.
    let conv_logs = brain.join("conv-alpha/.system_generated/logs");
    let chunks_dir = conv_logs.join("chunks/transcript");
    fs::create_dir_all(&chunks_dir)?;

    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32| {
        today
            .and_hms_opt(0, 5, sec)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };
    let step1_running = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "RUNNING",
        "created_at": ts(1),
        "input_tokens": 1000,
        "cache_read_tokens": 0,
        "output_tokens": 50
    });
    let step1_done = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2),
        "model": "gemini-4-neon-high",
        "input_tokens": 14688,
        "cache_read_tokens": 0,
        "output_tokens": 558
    });
    let step3_done = json!({
        "step_index": 3,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(3),
        "input_tokens": 3239,
        "cache_read_tokens": 12631,
        "output_tokens": 394
    });

    let transcript = conv_logs.join("transcript.jsonl");
    // Write duplicate copies into transcript_full.jsonl and chunks/ to verify they are NOT read.
    let dup_payload = format!("{step1_done}\n{step3_done}\n");
    fs::write(conv_logs.join("transcript_full.jsonl"), &dup_payload)?;
    fs::write(chunks_dir.join("00000000.jsonl"), &dup_payload)?;

    // Write RUNNING + DONE + duplicate DONE + incomplete trailing line (no newline yet).
    let mut f = fs::File::create(&transcript)?;
    writeln!(f, "{step1_running}")?;
    writeln!(f, "{step1_done}")?;
    writeln!(f, "{step1_done}")?;
    write!(f, "{step3_done}")?;
    f.flush()?;

    // Only step 1 DONE should be counted (14688 + 0 + 558 = 15246); incomplete step 3 waits.
    let u1 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u1.status, SourceStatus::Active);
    assert_eq!(u1.today, Some(15246));
    assert_eq!(u1.latest, Some(15246));

    // Finish line for step 3 (3239 + 12631 + 394 = 16264); total = 31510.
    writeln!(f)?;
    f.flush()?;
    let start = Instant::now();
    loop {
        let u2 = mon.update(&store, "antigravity", &brain)?;
        anyhow::ensure!(start.elapsed() < Duration::from_secs(2), "missed step 3");
        if u2.today == Some(31510) {
            assert_eq!(u2.latest, Some(16264));
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Simulate app closed: drop monitor, create a second conversation with a real-zero step + normal step.
    drop(mon);
    let conv2_logs = brain.join("conv-beta/.system_generated/logs");
    fs::create_dir_all(&conv2_logs)?;
    let step_zero = json!({
        "step_index": 0,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(4),
        "input_tokens": 0,
        "cache_read_tokens": 0,
        "output_tokens": 0
    });
    let step_next = json!({
        "step_index": 2,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(5),
        "input_tokens": 2062,
        "cache_read_tokens": 15247,
        "output_tokens": 341
    });
    fs::write(
        conv2_logs.join("transcript.jsonl"),
        format!("{step_zero}\n{step_next}\n"),
    )?;

    // Reopen twice: catches up once (31510 + 0 + 17650 = 49160), second reopen does not duplicate.
    for _ in 0..2 {
        let u3 = Monitor::default().update(&store, "antigravity", &brain)?;
        assert_eq!(u3.status, SourceStatus::Active);
        assert_eq!(u3.today, Some(49160));
        assert_eq!(u3.latest, Some(17650));
    }

    // Separate provider isolation: Codex has no rows in this store.
    let codex_dir = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_dir)?;
    let codex_u = Monitor::default().update(&store, "codex", &codex_dir)?;
    assert_eq!(codex_u.today, None);

    // If brain directory disappears later, returns Waiting while preserving historical totals.
    fs::remove_dir_all(&brain)?;
    let preserved = Monitor::default().update(&store, "antigravity", &brain)?;
    assert_eq!(preserved.status, SourceStatus::Waiting);
    assert_eq!(preserved.today, Some(49160));
    assert_eq!(preserved.latest, Some(17650));

    // If path is a regular file (unreadable as directory), returns an error.
    fs::write(&brain, "not-a-dir")?;
    assert!(Monitor::default().update(&store, "antigravity", &brain).is_err());

    Ok(())
}

#[test]
fn antigravity_cache_zero_vs_missing_and_complete_backfill() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-cache/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let transcript = logs.join("transcript.jsonl");

    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(10, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };

    // Case 1: explicit cache_read_tokens = 0 -> complete total = 320.
    let explicit_zero = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 100),
        "input_tokens": 300,
        "cache_read_tokens": 0,
        "output_tokens": 20
    });
    fs::write(&transcript, format!("{explicit_zero}\n"))?;
    let mut mon = Monitor::default();
    let u1 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u1.today, Some(320));
    assert_eq!(u1.today_complete_steps, 1);
    assert_eq!(u1.today_incomplete_steps, 0);
    assert_eq!(u1.latest, Some(320));
    assert!(!u1.latest_incomplete);

    // Case 2: missing cache_read_tokens in a separate fresh store -> incomplete (not 320!).
    let dir2 = tempdir()?;
    let store2 = Store::open(&dir2.path().join("monitor.sqlite"))?;
    store2.settings(false, || vec![agent("antigravity", "installed")])?;
    store2.select(vec!["antigravity".into()])?;
    let brain2 = dir2.path().join("brain");
    let logs2 = brain2.join("conv-missing/.system_generated/logs");
    fs::create_dir_all(&logs2)?;
    let transcript2 = logs2.join("transcript.jsonl");

    let missing_cache = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 100),
        "input_tokens": 300,
        "output_tokens": 20
    });
    let mut f2 = fs::File::create(&transcript2)?;
    writeln!(f2, "{missing_cache}")?;
    f2.flush()?;

    let mut mon2 = Monitor::default();
    let u_missing = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_missing.today, None);
    assert_eq!(u_missing.today_complete_steps, 0);
    assert_eq!(u_missing.today_incomplete_steps, 1);
    assert_eq!(u_missing.latest, None);
    assert!(u_missing.latest_incomplete);
    let parts = u_missing.latest_parts.expect("expected known parts");
    assert_eq!((parts.input, parts.cache_read, parts.output), (Some(300), None, Some(20)));

    // Explicit null cache_read_tokens on same step keeps incomplete state.
    let null_cache = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 200),
        "input_tokens": 300,
        "cache_read_tokens": null,
        "output_tokens": 20
    });
    writeln!(f2, "{null_cache}")?;
    f2.flush()?;
    let u_null = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_null.today, None);
    assert_eq!(u_null.today_incomplete_steps, 1);
    assert!(u_null.latest_incomplete);

    // Same (conv, step) later reports cache_read_tokens = 100 -> upgrades to 420, still 1 row.
    let backfilled = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 300,
        "cache_read_tokens": 100,
        "output_tokens": 20
    });
    writeln!(f2, "{backfilled}")?;
    f2.flush()?;
    let u_full = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_full.today, Some(420));
    assert_eq!(u_full.today_complete_steps, 1);
    assert_eq!(u_full.today_incomplete_steps, 0);
    assert_eq!(u_full.latest, Some(420));
    assert!(!u_full.latest_incomplete);
    let count: i64 = store2
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count, 1);

    // Replaying an earlier incomplete record must NOT overwrite the confirmed complete value.
    writeln!(f2, "{missing_cache}")?;
    f2.flush()?;
    let u_replay = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_replay.today, Some(420));
    assert_eq!(u_replay.latest, Some(420));
    assert!(!u_replay.latest_incomplete);
    let count_after: i64 = store2
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count_after, 1);

    // Mixed day: adding a second incomplete step leaves today = Some(420) (complete steps only)
    // while flagging today_incomplete_steps = 1 and latest_incomplete = true.
    let step6_incomplete = json!({
        "step_index": 6,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(3, 0),
        "input_tokens": 150,
        "output_tokens": 10
    });
    writeln!(f2, "{step6_incomplete}")?;
    f2.flush()?;
    let u_mixed = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_mixed.today, Some(420));
    assert_eq!(u_mixed.today_complete_steps, 1);
    assert_eq!(u_mixed.today_incomplete_steps, 1);
    assert_eq!(u_mixed.latest, None);
    assert!(u_mixed.latest_incomplete);
    Ok(())
}

#[test]
fn antigravity_subsecond_latest_and_deterministic_tie_breaker() -> anyhow::Result<()> {
    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(10, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };

    let conv_a_event = json!({
        "step_index": 90,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 100),
        "input_tokens": 80,
        "cache_read_tokens": 10,
        "output_tokens": 10
    });
    let conv_b_event = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 900),
        "input_tokens": 150,
        "cache_read_tokens": 30,
        "output_tokens": 20
    });

    // Verify both import orders (A then B, and B then A) yield latest = 200 and today = 300.
    for order in [["conv-a", "conv-b"], ["conv-b", "conv-a"]] {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("monitor.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain)?;
        let mut mon = Monitor::default();

        for conv in order {
            let logs = brain.join(conv).join(".system_generated/logs");
            fs::create_dir_all(&logs)?;
            let ev = if conv == "conv-a" { &conv_a_event } else { &conv_b_event };
            fs::write(logs.join("transcript.jsonl"), format!("{ev}\n"))?;
            mon.update(&store, "antigravity", &brain)?;
        }

        let u = mon.update(&store, "antigravity", &brain)?;
        assert_eq!(u.latest, Some(200), "order={order:?}");
        assert_eq!(u.today, Some(300), "order={order:?}");
    }

    // Exact same timestamp tie-breaker: independent of import order and survives restart.
    let tie_a = json!({
        "step_index": 90,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 100,
        "cache_read_tokens": 0,
        "output_tokens": 11
    });
    let tie_b = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 200,
        "cache_read_tokens": 0,
        "output_tokens": 22
    });

    let mut observed = Vec::new();
    for order in [["conv-a", "conv-b"], ["conv-b", "conv-a"]] {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("monitor.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain)?;
        let mut mon = Monitor::default();

        for conv in order {
            let logs = brain.join(conv).join(".system_generated/logs");
            fs::create_dir_all(&logs)?;
            let ev = if conv == "conv-a" { &tie_a } else { &tie_b };
            fs::write(logs.join("transcript.jsonl"), format!("{ev}\n"))?;
            mon.update(&store, "antigravity", &brain)?;
        }

        let u1 = mon.update(&store, "antigravity", &brain)?;
        let u2 = Monitor::default().update(&store, "antigravity", &brain)?;
        assert_eq!(u1.latest, u2.latest);
        assert_eq!(u1.today, Some(333));
        assert_eq!(u2.today, Some(333));
        observed.push(u1.latest);
    }
    // session DESC picks conv-b (222) deterministically in both import orders.
    assert_eq!(observed, vec![Some(222), Some(222)]);
    Ok(())
}

#[test]
fn legacy_db_upgrade_backfills_antigravity_preserves_deleted_logs_and_keeps_codex_cursors()
-> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("monitor.sqlite");
    let brain = dir.path().join("brain");
    let codex_root = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_root)?;

    let today = chrono::Local::now().date_naive();
    let ts_dt = |hour: u32, sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(hour, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
    };

    // On-disk Antigravity logs:
    // - conv-a step 90 at 10:00:01.100 (100 tokens, complete)
    // - conv-b step 1  at 10:00:01.900 (200 tokens, complete)
    // - conv-c step 1  at 09:30:00.500 (input 300, output 20, missing cache_read -> incomplete)
    let logs_a = brain.join("conv-a/.system_generated/logs");
    let logs_b = brain.join("conv-b/.system_generated/logs");
    let logs_c = brain.join("conv-c/.system_generated/logs");
    fs::create_dir_all(&logs_a)?;
    fs::create_dir_all(&logs_b)?;
    fs::create_dir_all(&logs_c)?;

    let file_a = logs_a.join("transcript.jsonl");
    let file_b = logs_b.join("transcript.jsonl");
    let file_c = logs_c.join("transcript.jsonl");
    let codex_file = codex_root.join("rollout.jsonl");

    fs::write(
        &file_a,
        format!(
            "{}\n",
            json!({
                "step_index": 90, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(10, 1, 100).to_rfc3339(),
                "input_tokens": 80, "cache_read_tokens": 10, "output_tokens": 10
            })
        ),
    )?;
    fs::write(
        &file_b,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(10, 1, 900).to_rfc3339(),
                "input_tokens": 150, "cache_read_tokens": 30, "output_tokens": 20
            })
        ),
    )?;
    fs::write(
        &file_c,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(9, 30, 500).to_rfc3339(),
                "input_tokens": 300, "output_tokens": 20
            })
        ),
    )?;
    fs::write(
        &codex_file,
        format!(
            "{}\n",
            json!({
                "type": "token_usage_record",
                "timestamp": ts_dt(8, 0, 0).to_rfc3339(),
                "payload": {"session_id": "s1", "response_id": "r1", "usage": {"total_tokens": 5000}}
            })
        ),
    )?;

    // Build an authentic legacy v0 SQLite database (5-column usage table, EOF cursors).
    {
        let con = rusqlite::Connection::open(&db_path)?;
        con.execute_batch(
            "PRAGMA user_version=0;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;

        // Seed legacy rows: Codex (5000), Antigravity A/B/C (on disk), Antigravity D (source log deleted!).
        con.execute(
            "INSERT INTO usage VALUES('codex','s1','r1',?1,5000)",
            [ts_dt(8, 0, 0).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-a','90',?1,100)",
            [ts_dt(10, 1, 100).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-b','1',?1,200)",
            [ts_dt(10, 1, 900).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-c','1',?1,320)",
            [ts_dt(9, 30, 500).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-deleted','1',?1,777)",
            [ts_dt(9, 0, 0).timestamp()],
        )?;

        // Seed EOF cursors for Codex and Antigravity using real file fingerprints & exact lengths.
        con.execute(
            "INSERT INTO cursors VALUES('codex', ?1, 'codex-fp', ?2)",
            rusqlite::params![codex_file.to_string_lossy(), codex_file.metadata()?.len() as i64],
        )?;
        for f in [&file_a, &file_b, &file_c] {
            con.execute(
                "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
                rusqlite::params![
                    f.to_string_lossy(),
                    test_file_id(f)?,
                    f.metadata()?.len() as i64
                ],
            )?;
        }
    }

    // Open with new Store -> upgrades schema to v5, preserves Antigravity cursors & queues repair tasks.
    let store = Store::open(&db_path)?;
    {
        let con = store.connect()?;
        let ver: i32 = con.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(ver, 6);
        assert!(store.antigravity_repair_pending()?);
        let codex_cursor_fp: String = con.query_row(
            "SELECT fingerprint FROM cursors WHERE provider='codex'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(codex_cursor_fp, "codex-fp", "Codex cursor must stay untouched");
        let ag_cursors_before: i64 = con.query_row(
            "SELECT COUNT(*) FROM cursors WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(ag_cursors_before, 3, "Antigravity cursors should be preserved to prove collected prefix");
    }

    // Run Antigravity monitor after upgrade:
    // - conv-a & conv-b recover sub-second precision -> latest = 200 (conv-b 10:00:01.900)
    // - conv-c is corrected from false-complete 320 to incomplete (today_incomplete_steps = 1)
    // - conv-deleted is preserved SEPARATELY as legacy history (777 tokens, today_legacy_steps = 1)
    // - verified complete today sum = 100 + 200 = 300 (legacy 777 is NOT mixed into verified sum!)
    let u_after = Monitor::default().update(&store, "antigravity", &brain)?;
    assert!(!store.antigravity_repair_pending()?);
    assert_eq!(u_after.latest, Some(200));
    assert_eq!(u_after.today, Some(300));
    assert_eq!(u_after.today_complete_steps, 2);
    assert_eq!(u_after.today_incomplete_steps, 1);
    assert_eq!(u_after.today_legacy_steps, 1);
    assert_eq!(u_after.today_legacy_tokens, Some(777));
    assert!(!u_after.latest_incomplete);

    let ag_row_count: i64 = store
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(ag_row_count, 4);

    // Reopening Store and Monitor again must NOT clear cursors or double history.
    let store_reopened = Store::open(&db_path)?;
    let ag_cursors_after: i64 = store_reopened.connect()?.query_row(
        "SELECT COUNT(*) FROM cursors WHERE provider='antigravity'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(ag_cursors_after, 3);

    let u_reopened = Monitor::default().update(&store_reopened, "antigravity", &brain)?;
    assert_eq!(u_reopened.latest, Some(200));
    assert_eq!(u_reopened.today, Some(300));
    assert_eq!(u_reopened.today_incomplete_steps, 1);
    assert_eq!(u_reopened.today_legacy_steps, 1);
    assert_eq!(u_reopened.today_legacy_tokens, Some(777));

    // Also verify History daily/weekly/monthly totals do not double across reopens.
    let hist_day = history::query(&store_reopened, Granularity::Day, 0, Some("antigravity"), None, None)?;
    assert_eq!(hist_day.buckets.len(), 1);
    let today_bucket = hist_day.buckets.first().unwrap();
    assert_eq!(today_bucket.verified_tokens, Some(300));
    assert_eq!(today_bucket.incomplete_steps, 1);
    assert_eq!(today_bucket.legacy_steps, 1);
    assert_eq!(today_bucket.legacy_tokens, Some(777));

    // Codex history remained untouched (5000, legacy = 0).
    let (codex_tokens, codex_legacy): (i64, i64) = store_reopened.connect()?.query_row(
        "SELECT tokens, legacy FROM usage WHERE provider='codex'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((codex_tokens, codex_legacy), (5000, 0));

    Ok(())
}

#[test]
fn history_cross_day_week_month_and_cross_year_boundaries() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate, TimeZone};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions)?;
    let write_ev = |id: &str, rfc: &str, tokens: i64| {
        json!({
            "type": "token_usage_record",
            "timestamp": rfc,
            "payload": {"session_id": "s1", "response_id": id, "usage": {"total_tokens": tokens}}
        })
    };
    fs::write(
        sessions.join("boundaries.jsonl"),
        [
            // Cross-year: 2025-12-31 23:59:59+08:00 (50) and 2026-01-01 00:00:00+08:00 (75)
            write_ev("y1", "2025-12-31T23:59:59+08:00", 50),
            write_ev("y2", "2026-01-01T00:00:00+08:00", 75),
            // Acceptance example: 2026-11-29 Sun 23:59:59 (100), 11-30 Mon 00:00:00 (200), 12-01 Tue 00:00:00 (300)
            write_ev("r1", "2026-11-29T23:59:59+08:00", 100),
            write_ev("r2", "2026-11-30T00:00:00+08:00", 200),
            write_ev("r3", "2026-12-01T00:00:00+08:00", 300),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "codex", &sessions)?;

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();

    // Daily single-day pages: offset 0 = 12-01 (300), offset 1 = 11-30 (200), offset 2 = 11-29 (100)
    let day0 = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    assert_eq!(day0.buckets.len(), 1);
    assert_eq!(day0.range_start, "2026-12-01");
    assert_eq!(day0.summary.verified_tokens, Some(300));
    assert!(day0.has_older);
    assert!(!day0.has_newer);
    let day1 = history::query_with_tz(&store, Granularity::Day, 1, None, None, None, today, &tz)?;
    assert_eq!(day1.range_start, "2026-11-30");
    assert_eq!(day1.summary.verified_tokens, Some(200));
    let day2 = history::query_with_tz(&store, Granularity::Day, 2, None, None, None, today, &tz)?;
    assert_eq!(day2.range_start, "2026-11-29");
    assert_eq!(day2.summary.verified_tokens, Some(100));

    // Weekly single-week pages (Monday start): offset 0 = 11-30..12-01 (500), offset 1 = 11-23..11-29 (100)
    let week0 = history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(week0.range_start, "2026-11-30");
    assert_eq!(week0.range_end, "2026-12-01");
    assert!(week0.is_current);
    assert_eq!(week0.summary.verified_tokens, Some(500));
    let week1 = history::query_with_tz(&store, Granularity::Week, 1, None, None, None, today, &tz)?;
    assert_eq!(week1.range_start, "2026-11-23");
    assert_eq!(week1.range_end, "2026-11-29");
    assert!(!week1.is_current);
    assert_eq!(week1.summary.verified_tokens, Some(100));

    // Monthly single-month pages: 2026-12 = 300, 2026-11 = 300, 2026-01 = 75, 2025-12 = 50
    let dec26 = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(dec26.range_start, "2026-12-01");
    assert_eq!(dec26.range_end, "2026-12-01");
    assert_eq!(dec26.summary.verified_tokens, Some(300));
    let nov26 = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
    assert_eq!(nov26.range_start, "2026-11-01");
    assert_eq!(nov26.range_end, "2026-11-30");
    assert_eq!(nov26.summary.verified_tokens, Some(300));
    let jan26 = history::query_with_tz(&store, Granularity::Month, 11, None, None, None, today, &tz)?;
    assert_eq!(jan26.summary.verified_tokens, Some(75));
    let dec25 = history::query_with_tz(&store, Granularity::Month, 12, None, None, None, today, &tz)?;
    assert!(dec25.has_newer);
    assert!(!dec25.has_older);
    assert_eq!(dec25.summary.verified_tokens, Some(50));
    let _ = tz.timestamp_opt(0, 0);
    Ok(())
}

#[test]
fn history_partial_backfill_merges_fields_and_preserves_original_date() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-backfill/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let transcript = logs.join("transcript.jsonl");

    // Nov 29 reports input=300, output=20 (no cache_read_tokens)
    let mut f = fs::File::create(&transcript)?;
    writeln!(
        f,
        "{}",
        json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 300,
            "output_tokens": 20
        })
    )?;
    f.flush()?;

    let mut mon = Monitor::default();
    mon.update(&store, "antigravity", &brain)?;

    // Dec 1 backfills ONLY cache_read_tokens=100 for the same step
    writeln!(
        f,
        "{}",
        json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-12-01T10:00:00+08:00",
            "cache_read_tokens": 100
        })
    )?;
    f.flush()?;
    mon.update(&store, "antigravity", &brain)?;

    let count: i64 = store
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count, 1);

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
    let day_dec1 = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    assert_eq!(day_dec1.summary.verified_tokens, None);
    let day_nov29 = history::query_with_tz(&store, Granularity::Day, 2, None, None, None, today, &tz)?;
    assert_eq!(day_nov29.summary.verified_tokens, Some(420));

    let week_curr = history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(week_curr.summary.verified_tokens, None);
    let week_prev = history::query_with_tz(&store, Granularity::Week, 1, None, None, None, today, &tz)?;
    assert_eq!(week_prev.summary.verified_tokens, Some(420));

    let month_dec = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(month_dec.summary.verified_tokens, None);
    let month_nov = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
    assert_eq!(month_nov.summary.verified_tokens, Some(420));
    Ok(())
}

#[test]
fn history_distinguishes_empty_zero_all_missing_legacy_and_disabled_provider() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || {
        vec![agent("codex", "installed"), agent("antigravity", "installed")]
    })?;
    store.select(vec!["codex".into(), "antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-states/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    // Day 2026-11-21: empty (no record)
    // Day 2026-11-22: explicit zero (0, 0, 0)
    // Day 2026-11-23: DONE step with all 3 token fields missing
    // Day 2026-11-24: legacy-only row inserted directly (simulating deleted source log)
    // Day 2026-11-25: Codex 100 + Antigravity 200 -> All = 300
    fs::write(
        logs.join("transcript.jsonl"),
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-22T10:00:00+08:00", "input_tokens": 0, "cache_read_tokens": 0, "output_tokens": 0}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-23T10:00:00+08:00"}),
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-25T10:00:00+08:00", "input_tokens": 150, "cache_read_tokens": 30, "output_tokens": 20}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "antigravity", &brain)?;

    let codex_dir = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_dir)?;
    fs::write(
        codex_dir.join("rollout.jsonl"),
        format!(
            "{}\n",
            json!({
                "type": "token_usage_record",
                "timestamp": "2026-11-25T11:00:00+08:00",
                "payload": {"session_id": "s1", "response_id": "r1", "usage": {"total_tokens": 100}}
            })
        ),
    )?;
    Monitor::default().update(&store, "codex", &codex_dir)?;

    // Insert a legacy-only Antigravity row on 2026-11-24
    let legacy_ts = chrono::DateTime::parse_from_rfc3339("2026-11-24T12:00:00+08:00")?.timestamp();
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('antigravity','deleted-conv','1',?1,777,NULL,1,1)",
        [legacy_ts],
    )?;

    let today = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let page = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    let bucket = |k: &str| page.buckets.iter().find(|b| b.key == k).unwrap();

    // 1. Empty
    let b21 = bucket("2026-11-21");
    assert_eq!((b21.verified_tokens, b21.incomplete_steps, b21.legacy_steps), (None, 0, 0));
    // 2. Explicit zero
    let b22 = bucket("2026-11-22");
    assert_eq!((b22.verified_tokens, b22.verified_steps, b22.incomplete_steps), (Some(0), 1, 0));
    // 3. All 3 fields missing -> preserved as incomplete with incomplete_known = None
    let b23 = bucket("2026-11-23");
    assert_eq!(b23.verified_tokens, None);
    assert_eq!(b23.incomplete_steps, 1);
    assert_eq!(b23.incomplete_known, None);
    // 4. Legacy-only -> separated from verified_tokens, legacy_tokens = Some(777)
    let b24 = bucket("2026-11-24");
    assert_eq!(b24.verified_tokens, None);
    assert_eq!(b24.legacy_steps, 1);
    assert_eq!(b24.legacy_tokens, Some(777));
    // 5. Codex 100 + Antigravity 200 -> All = 300
    let b25 = bucket("2026-11-25");
    assert_eq!(b25.verified_tokens, Some(300));
    assert_eq!(b25.providers.len(), 2);

    // Disable Antigravity: its saved 200 must still be queryable without re-enabling collection!
    store.select(vec!["codex".into()])?;
    let ag_only = history::query_with_tz(&store, Granularity::Month, 0, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!(
        ag_only.buckets.iter().find(|b| b.key == "2026-11-25").unwrap().verified_tokens,
        Some(200)
    );
    assert!(
        !ag_only
            .available_providers
            .iter()
            .find(|p| p.id == "antigravity")
            .unwrap()
            .enabled
    );
    assert!(store.enabled_agent("antigravity").is_err());
    Ok(())
}

#[test]
fn history_dst_23h_and_25h_days_do_not_assume_86400_seconds() -> anyhow::Result<()> {
    use chrono::{FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

    // Minimal US-like DST timezone:
    // - Spring forward: 2026-03-08 02:00 EST (-5) -> 03:00 EDT (-4) (23-hour day)
    // - Fall back:      2026-11-01 02:00 EDT (-4) -> 01:00 EST (-5) (25-hour day)
    #[derive(Clone, Copy)]
    struct MockDstTz;
    impl MockDstTz {
        fn is_dst_utc(utc_ts: i64) -> bool {
            // 2026-03-08 07:00:00Z .. 2026-11-01 06:00:00Z
            let start = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap().and_hms_opt(7, 0, 0).unwrap().and_utc().timestamp();
            let end = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap().and_hms_opt(6, 0, 0).unwrap().and_utc().timestamp();
            utc_ts >= start && utc_ts < end
        }
    }
    impl TimeZone for MockDstTz {
        type Offset = FixedOffset;
        fn from_offset(_offset: &Self::Offset) -> Self { MockDstTz }
        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let est = FixedOffset::west_opt(5 * 3600).unwrap();
            let edt = FixedOffset::west_opt(4 * 3600).unwrap();
            let est_utc = local.and_utc().timestamp() + 5 * 3600;
            let edt_utc = local.and_utc().timestamp() + 4 * 3600;
            let valid_est = !Self::is_dst_utc(est_utc);
            let valid_edt = Self::is_dst_utc(edt_utc);
            match (valid_edt, valid_est) {
                (true, false) => LocalResult::Single(edt),
                (false, true) => LocalResult::Single(est),
                (true, true) => LocalResult::Ambiguous(edt, est),
                (false, false) => LocalResult::None,
            }
        }
        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            if Self::is_dst_utc(utc.and_utc().timestamp()) {
                FixedOffset::west_opt(4 * 3600).unwrap()
            } else {
                FixedOffset::west_opt(5 * 3600).unwrap()
            }
        }
    }

    // Verify 23-hour spring-forward day and 25-hour fall-back day boundaries!
    let s1 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), &MockDstTz)?;
    let s2 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(), &MockDstTz)?;
    assert_eq!(s2 - s1, 23 * 3600);

    let f1 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), &MockDstTz)?;
    let f2 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(), &MockDstTz)?;
    assert_eq!(f2 - f1, 25 * 3600);

    // Event at 23:30 EST on the 25-hour day (24.5 hours after 00:00 EDT!) must STILL land in 2026-11-01!
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;
    let late_25h_ts = f1 + 24 * 3600 + 1800; // 23:30 EST on 2026-11-01
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('codex','s1','dst1',?1,456,?2,1,0)",
        rusqlite::params![late_25h_ts, late_25h_ts * 1_000_000_000],
    )?;

    let page = history::query_with_tz(
        &store,
        Granularity::Month,
        0,
        None,
        None,
        None,
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
        &MockDstTz,
    )?;
    assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-01").unwrap().verified_tokens, Some(456));
    assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-02").unwrap().verified_tokens, None);
    Ok(())
}

#[test]
fn antigravity_out_of_order_field_merge_and_restart_persistence() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();

    // Sequence 1: Nov 27 complete 300 -> Nov 29 complete 500 -> replay Nov 28 complete 400.
    // Final: 1 step, 500 tokens attributed to Nov 27; replaying again yields identical state.
    {
        let dir = tempdir()?;
        let db_path = dir.path().join("seq1.sqlite");
        let store = Store::open(&db_path)?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;

        let brain = dir.path().join("brain");
        let logs = brain.join("conv-seq1/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let transcript = logs.join("transcript.jsonl");
        let mut f = fs::File::create(&transcript)?;

        let e27 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-27T10:00:00+08:00",
            "input_tokens": 200, "cache_read_tokens": 50, "output_tokens": 50
        });
        let e29 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 350, "cache_read_tokens": 100, "output_tokens": 50
        });
        let e28 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-28T10:00:00+08:00",
            "input_tokens": 250, "cache_read_tokens": 100, "output_tokens": 50
        });

        writeln!(f, "{e27}")?;
        writeln!(f, "{e29}")?;
        writeln!(f, "{e28}")?;
        f.flush()?;

        let mut mon = Monitor::default();
        mon.update(&store, "antigravity", &brain)?;

        // Replay Nov 28 again
        writeln!(f, "{e28}")?;
        f.flush()?;
        mon.update(&store, "antigravity", &brain)?;

        let count: i64 = store.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);

        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let page = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-27").unwrap().verified_tokens, Some(500));
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap().verified_tokens, None);
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens, None);
    }

    // Sequence 2: Dec 1 complete 420 -> restart -> Nov 29 earlier partial record.
    // Final: keeps complete 420, attributed to Nov 29; Dec 1 no longer has this step.
    {
        let dir = tempdir()?;
        let db_path = dir.path().join("seq2.sqlite");
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-seq2/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let transcript = logs.join("transcript.jsonl");

        {
            let store = Store::open(&db_path)?;
            store.settings(false, || vec![agent("antigravity", "installed")])?;
            store.select(vec!["antigravity".into()])?;
            fs::write(
                &transcript,
                format!(
                    "{}\n",
                    json!({
                        "step_index": 9, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": "2026-12-01T10:00:00+08:00",
                        "input_tokens": 300, "cache_read_tokens": 100, "output_tokens": 20
                    })
                ),
            )?;
            Monitor::default().update(&store, "antigravity", &brain)?;
        }

        // Restart Store & Monitor, then append earlier Nov 29 partial record
        let store = Store::open(&db_path)?;
        let mut f = fs::OpenOptions::new().append(true).open(&transcript)?;
        writeln!(
            f,
            "{}",
            json!({
                "step_index": 9, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": "2026-11-29T10:00:00+08:00",
                "input_tokens": 280, "output_tokens": 15
            })
        )?;
        f.flush()?;
        Monitor::default().update(&store, "antigravity", &brain)?;

        let count: i64 = store.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);

        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let day_nov29 = history::query_with_tz(&store, Granularity::Day, 2, None, None, None, today, &tz)?;
        assert_eq!(day_nov29.summary.verified_tokens, Some(420));
        let day_dec01 = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
        assert_eq!(day_dec01.summary.verified_tokens, None);

        let month_nov = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
        assert_eq!(month_nov.summary.verified_tokens, Some(420));
        let month_dec = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
        assert_eq!(month_dec.summary.verified_tokens, None);
    }

    // Explicit 0 is a valid newer value and must not be overwritten by later missing field.
    {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("zero.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-zero/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        fs::write(
            logs.join("transcript.jsonl"),
            [
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-30T09:00:00+08:00", "cache_read_tokens": 0}),
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T09:00:00+08:00", "input_tokens": 310}),
            ]
            .map(|v| v.to_string())
            .join("\n")
                + "\n",
        )?;
        Monitor::default().update(&store, "antigravity", &brain)?;
        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let nov_page = history::query_with_tz(&store, Granularity::Month, 1, Some("antigravity"), None, None, today, &tz)?;
        assert_eq!(nov_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens, Some(330));
        assert_eq!(nov_page.buckets.iter().find(|b| b.key == "2026-11-30").unwrap().verified_tokens, None);
        let dec_page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
        assert_eq!(dec_page.summary.verified_tokens, None);
    }

    Ok(())
}

#[test]
fn v2_upgrade_repairs_antigravity_and_resumes_after_interruption() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let db_v2 = dir.path().join("from_v2.sqlite");
    let db_fresh = dir.path().join("fresh_v3.sqlite");
    let brain = dir.path().join("brain");
    let codex_root = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_root)?;

    let logs1 = brain.join("conv-main/.system_generated/logs");
    let logs2 = brain.join("conv-second/.system_generated/logs");
    fs::create_dir_all(&logs1)?;
    fs::create_dir_all(&logs2)?;
    let t1 = logs1.join("transcript.jsonl");
    let t2 = logs2.join("transcript.jsonl");

    // conv-main: Step A (DONE, all 3 fields missing) + Step B (Nov 29 in=300, out=20; Dec 1 cache_read=100)
    fs::write(
        &t1,
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00"}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    fs::write(
        &t2,
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-30T11:00:00+08:00", "input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 25})
        ),
    )?;

    // Build a v2 database matching 7d76bf1's schema & EOF state (Step A skipped, Step B overwritten with Dec 1 partial 100).
    {
        let con = rusqlite::Connection::open(&db_v2)?;
        con.execute_batch(
            "PRAGMA user_version=2;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        let nov29_a = chrono::DateTime::parse_from_rfc3339("2026-11-29T09:00:00+08:00")?;
        let nov29_b = chrono::DateTime::parse_from_rfc3339("2026-11-29T10:00:00+08:00")?;
        let nov30 = chrono::DateTime::parse_from_rfc3339("2026-11-30T11:00:00+08:00")?;
        let nov28 = chrono::DateTime::parse_from_rfc3339("2026-11-28T10:00:00+08:00")?;
        let nov29 = chrono::DateTime::parse_from_rfc3339("2026-11-29T08:00:00+08:00")?;
        // 7b1ecd9-written v2 conv-main (Step 1 incomplete 0, Step 2 complete 420) + conv-second (175) + deleted-log (777) + Codex (5000).
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-main','1',?1,0,?2,0,NULL,NULL,NULL,0)",
            rusqlite::params![nov29_a.timestamp(), nov29_a.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-main','2',?1,420,?2,1,300,100,20,0)",
            rusqlite::params![nov29_b.timestamp(), nov29_b.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-second','1',?1,175,?2,1,100,50,25,0)",
            rusqlite::params![nov30.timestamp(), nov30.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-deleted','1',?1,777,?2,1,700,50,27,0)",
            rusqlite::params![nov28.timestamp(), nov28.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('codex','s1','r1',?1,5000,?2,1,NULL,NULL,NULL,0)",
            rusqlite::params![nov29.timestamp(), nov29.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('codex', '/fake/codex.jsonl', 'codex-fp', 160)",
            [],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
            rusqlite::params![
                t1.to_string_lossy(),
                test_file_id(&t1)?,
                t1.metadata()?.len() as i64
            ],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
            rusqlite::params![
                t2.to_string_lossy(),
                test_file_id(&t2)?,
                t2.metadata()?.len() as i64
            ],
        )?;
    }

    // Open with new Store (v4 migration runs) and simulate real mid-repair interruption via task permission error on t2.
    let store = Store::open(&db_v2)?;
    assert!(store.antigravity_repair_pending()?);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let orig_perms = fs::metadata(&t2)?.permissions();
        fs::set_permissions(&t2, fs::Permissions::from_mode(0o000))?;
        let first_attempt = usage::repair_antigravity_history(&store, &brain);
        fs::set_permissions(&t2, orig_perms)?;
        assert!(first_attempt.is_err(), "unreadable second transcript should pause repair");
        assert!(store.antigravity_repair_pending()?);
        let done_count: i64 = store.connect()?.query_row(
            "SELECT COUNT(*) FROM antigravity_repair_tasks WHERE done=1",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(done_count, 1, "conv-main task should stay committed as done=1 across crash");
    }

    // Reopen Store & resume repair without Monitor::update: skips done conv-main, repairs conv-second, marks v4 done!
    let store = Store::open(&db_v2)?;
    assert!(store.antigravity_repair_pending()?);
    usage::repair_antigravity_history(&store, &brain)?;
    assert!(!store.antigravity_repair_pending()?);

    // Build a fresh v3 DB from the exact same on-disk transcripts for field-by-field comparison.
    let fresh = Store::open(&db_fresh)?;
    fresh.settings(false, || vec![agent("antigravity", "installed")])?;
    fresh.select(vec!["antigravity".into()])?;
    Monitor::default().update(&fresh, "antigravity", &brain)?;

    type RowDump = (String, String, i64, Option<i64>, i64, i64, Option<i64>, Option<i64>, Option<i64>, i64);
    let dump = |s: &Store| -> anyhow::Result<Vec<RowDump>> {
        let con = s.connect()?;
        let mut stmt = con.prepare(
            "SELECT session, response, at, at_ns, tokens, complete, input_tokens, cache_read_tokens, output_tokens, legacy
             FROM usage WHERE provider='antigravity' AND session != 'conv-deleted' ORDER BY session, response",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    };
    assert_eq!(dump(&store)?, dump(&fresh)?);

    // Reopen twice more: zero duplication, repair stays done, Codex untouched, conv-deleted preserved as legacy.
    for _ in 0..2 {
        let s = Store::open(&db_v2)?;
        assert!(!s.antigravity_repair_pending()?);
        Monitor::default().update(&s, "antigravity", &brain)?;
        assert_eq!(dump(&s)?, dump(&fresh)?);
    }

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
    let nov_page = history::query_with_tz(&store, Granularity::Month, 1, Some("antigravity"), None, None, today, &tz)?;
    let b28 = nov_page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap();
    assert_eq!((b28.verified_tokens, b28.legacy_steps, b28.legacy_tokens), (None, 1, Some(777)));
    let b29 = nov_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap();
    assert_eq!((b29.verified_tokens, b29.verified_steps, b29.incomplete_steps, b29.incomplete_known.clone()), (Some(420), 1, 1, None));
    let b30 = nov_page.buckets.iter().find(|b| b.key == "2026-11-30").unwrap();
    assert_eq!(b30.verified_tokens, Some(175));
    let dec_page = history::query_with_tz(&store, Granularity::Day, 0, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!((dec_page.summary.verified_tokens, dec_page.summary.incomplete_steps), (None, 0));

    let codex_fp: String = store.connect()?.query_row("SELECT fingerprint FROM cursors WHERE provider='codex'", [], |r| r.get(0))?;
    assert_eq!(codex_fp, "codex-fp");
    Ok(())
}

#[test]
fn history_year_granularity_monthly_drilldown_and_current_end_date_capping() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions)?;
    let write_ev = |id: &str, rfc: &str, tokens: i64| {
        json!({
            "type": "token_usage_record",
            "timestamp": rfc,
            "payload": {"session_id": "s1", "response_id": id, "usage": {"total_tokens": tokens}}
        })
    };
    fs::write(
        sessions.join("yearly.jsonl"),
        [
            write_ev("y1", "2025-12-31T23:59:59+08:00", 50),
            write_ev("y2", "2026-01-01T00:00:00+08:00", 75),
            write_ev("y3", "2026-11-25T12:00:00+08:00", 100),
            write_ev("y4", "2026-11-26T12:00:00+08:00", 200),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "codex", &sessions)?;

    let today = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();

    // Week: single current week 2026-11-23..2026-11-26 (daily buckets summed to 300), previous week 11-16..11-22 empty.
    let week_page =
        history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(week_page.range_start, "2026-11-23");
    assert_eq!(week_page.range_end, "2026-11-26");
    assert!(week_page.is_current);
    assert_eq!(week_page.buckets.len(), 4);
    assert_eq!(week_page.summary.verified_tokens, Some(300));

    // Month: single current month 2026-11-01..2026-11-26 (26 daily buckets summed to 300).
    let month_page =
        history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(month_page.range_start, "2026-11-01");
    assert_eq!(month_page.range_end, "2026-11-26");
    assert!(month_page.is_current);
    assert_eq!(month_page.buckets.len(), 26);
    assert_eq!(month_page.summary.verified_tokens, Some(300));

    // Year: single current year 2026-01-01..2026-11-26 (11 monthly buckets summed to 375).
    let year_page =
        history::query_with_tz(&store, Granularity::Year, 0, None, None, None, today, &tz)?;
    assert_eq!(year_page.range_start, "2026-01-01");
    assert_eq!(year_page.range_end, "2026-11-26");
    assert!(year_page.is_current);
    assert_eq!(year_page.buckets.len(), 11);
    assert_eq!(year_page.summary.verified_tokens, Some(375));
    assert_eq!(year_page.buckets.iter().find(|b| b.key == "2026-01").unwrap().verified_tokens, Some(75));
    assert_eq!(year_page.buckets.iter().find(|b| b.key == "2026-11").unwrap().verified_tokens, Some(300));

    // Previous Year (offset = 1): full calendar year 2025-01-01..2025-12-31 (12 months, total 50).
    let prev_year =
        history::query_with_tz(&store, Granularity::Year, 1, None, None, None, today, &tz)?;
    assert_eq!(prev_year.range_start, "2025-01-01");
    assert_eq!(prev_year.range_end, "2025-12-31");
    assert!(!prev_year.is_current);
    assert_eq!(prev_year.buckets.len(), 12);
    assert_eq!(prev_year.summary.verified_tokens, Some(50));
    Ok(())
}

#[test]
fn v2_upgrade_disabled_antigravity_repairs_only_collected_prefix_and_resumes() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let db_v2 = dir.path().join("disabled_v2.sqlite");
    let brain = dir.path().join("brain");

    let logs1 = brain.join("conv-disabled/.system_generated/logs");
    let logs2 = brain.join("conv-deleted/.system_generated/logs");
    let logs_post_new = brain.join("conv-post-new/.system_generated/logs");
    fs::create_dir_all(&logs1)?;
    fs::create_dir_all(&logs2)?;
    fs::create_dir_all(&logs_post_new)?;
    let t1 = logs1.join("transcript.jsonl");
    let t2 = logs2.join("transcript.jsonl");

    // Pre-disable collected content written by 7b1ecd9:
    // - Step 1: DONE with all 3 token fields missing (saved as complete=0, 0)
    // - Step 2: Nov 29 input=300, output=20 + Dec 1 cache_read=100 (merged to Nov 29 420, complete=1)
    fs::write(
        &t1,
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00"}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    let collected_end_pos = t1.metadata()?.len() as i64;
    let fp1 = test_file_id(&t1)?;

    // Deleted transcript recorded under v2 (no longer exists when upgrading)
    fs::write(&t2, "{}\n")?;
    let fp2 = test_file_id(&t2)?;
    let pos2 = t2.metadata()?.len() as i64;
    fs::remove_dir_all(brain.join("conv-deleted"))?;

    // Build v2 database matching 7b1ecd9 where Antigravity was ALREADY DISABLED before upgrading.
    {
        let con = rusqlite::Connection::open(&db_v2)?;
        con.execute_batch(
            "PRAGMA user_version=2;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        let nov29_a = chrono::DateTime::parse_from_rfc3339("2026-11-29T09:00:00+08:00")?;
        let nov29_b = chrono::DateTime::parse_from_rfc3339("2026-11-29T10:00:00+08:00")?;
        let nov28 = chrono::DateTime::parse_from_rfc3339("2026-11-28T10:00:00+08:00")?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-disabled','1',?1,0,?2,0,NULL,NULL,NULL,0)",
            rusqlite::params![nov29_a.timestamp(), nov29_a.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-disabled','2',?1,420,?2,1,300,100,20,0)",
            rusqlite::params![nov29_b.timestamp(), nov29_b.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-deleted','1',?1,777,?2,1,700,50,27,0)",
            rusqlite::params![nov28.timestamp(), nov28.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
            rusqlite::params![t1.to_string_lossy(), fp1, collected_end_pos],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
            rusqlite::params![t2.to_string_lossy(), fp2, pos2],
        )?;
    }

    // Post-disable activity BEFORE upgrade: append Step 3 to t1 AND create brand-new conversation conv-post-new.
    {
        let mut f = fs::OpenOptions::new().append(true).open(&t1)?;
        writeln!(
            f,
            "{}",
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T10:00:00+08:00", "input_tokens": 900, "cache_read_tokens": 50, "output_tokens": 50})
        )?;
    }
    fs::write(
        logs_post_new.join("transcript.jsonl"),
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T11:00:00+08:00", "input_tokens": 600, "cache_read_tokens": 100, "output_tokens": 100})
        ),
    )?;

    // Upgrade at startup while Antigravity remains disabled.
    let store = Store::open(&db_v2)?;
    assert!(store.antigravity_repair_pending()?);
    usage::repair_antigravity_history(&store, &brain)?;
    assert!(!store.antigravity_repair_pending()?);
    assert!(store.enabled_agent("antigravity").is_err());

    let today = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
    let nov_page = history::query_with_tz(&store, Granularity::Month, 1, Some("antigravity"), None, None, today, &tz)?;
    let b28 = nov_page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap();
    assert_eq!((b28.verified_tokens, b28.legacy_steps, b28.legacy_tokens), (None, 1, Some(777)));
    let b29 = nov_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap();
    assert_eq!((b29.verified_tokens, b29.verified_steps, b29.incomplete_steps, b29.incomplete_known.clone()), (Some(420), 1, 1, None));
    let dec_page = history::query_with_tz(&store, Granularity::Month, 0, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!((dec_page.summary.verified_tokens, dec_page.summary.incomplete_steps), (None, 0));

    // Reopen twice: zero duplication.
    for _ in 0..2 {
        let s = Store::open(&db_v2)?;
        assert!(!s.antigravity_repair_pending()?);
        usage::repair_antigravity_history(&s, &brain)?;
        let count: i64 = s.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 3); // Step 1, Step 2, conv-deleted
    }

    Ok(())
}

#[test]
fn v3_cursorless_disabled_antigravity_repairs_verifiable_and_keeps_unverifiable_legacy() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let db_v3 = dir.path().join("disabled_v3.sqlite");
    let brain = dir.path().join("brain");

    let logs_ok = brain.join("conv-verifiable/.system_generated/logs");
    let logs_mod = brain.join("conv-tampered/.system_generated/logs");
    fs::create_dir_all(&logs_ok)?;
    fs::create_dir_all(&logs_mod)?;
    let t_ok = logs_ok.join("transcript.jsonl");
    let t_mod = logs_mod.join("transcript.jsonl");

    fs::write(
        &t_ok,
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00"}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T10:00:00+08:00", "input_tokens": 900, "cache_read_tokens": 49, "output_tokens": 50}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    fs::write(
        &t_mod,
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-28T09:00:00+08:00", "input_tokens": 111, "cache_read_tokens": 222, "output_tokens": 333})
        ),
    )?;

    {
        let con = rusqlite::Connection::open(&db_v3)?;
        con.execute_batch(
            "PRAGMA user_version=3;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE usage(
                provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL,
                at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER,
                cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0,
                input_at_ns INTEGER, cache_read_at_ns INTEGER, output_at_ns INTEGER,
                PRIMARY KEY(provider,session,response)
             );
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        con.execute("INSERT INTO meta VALUES('antigravity_repair_version', '2')", [])?;

        let nov29_a = chrono::DateTime::parse_from_rfc3339("2026-11-29T09:00:00+08:00")?;
        let nov29_b = chrono::DateTime::parse_from_rfc3339("2026-11-29T10:00:00+08:00")?;
        let nov28 = chrono::DateTime::parse_from_rfc3339("2026-11-28T09:00:00+08:00")?;
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
             VALUES('antigravity','conv-verifiable','1',?1,0,?2,0,NULL,NULL,NULL,1)",
            rusqlite::params![nov29_a.timestamp(), nov29_a.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
             VALUES('antigravity','conv-verifiable','2',?1,420,?2,1,300,100,20,1)",
            rusqlite::params![nov29_b.timestamp(), nov29_b.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
             VALUES('antigravity','conv-tampered','1',?1,555,?2,1,500,50,5,1)",
            rusqlite::params![nov28.timestamp(), nov28.timestamp_nanos_opt()],
        )?;
    }

    let store = Store::open(&db_v3)?;
    assert!(store.antigravity_repair_pending()?);
    usage::repair_antigravity_history(&store, &brain)?;
    assert!(!store.antigravity_repair_pending()?);
    assert!(store.enabled_agent("antigravity").is_err());

    let today = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
    let nov_page = history::query_with_tz(&store, Granularity::Month, 1, Some("antigravity"), None, None, today, &tz)?;
    let b28 = nov_page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap();
    assert_eq!((b28.verified_tokens, b28.legacy_steps, b28.legacy_tokens), (None, 1, Some(555)));
    let b29 = nov_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap();
    assert_eq!((b29.verified_tokens, b29.verified_steps, b29.incomplete_steps), (Some(420), 1, 1));
    let dec_page = history::query_with_tz(&store, Granularity::Day, 0, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!(dec_page.summary.verified_tokens, None);

    for _ in 0..2 {
        let s = Store::open(&db_v3)?;
        assert!(!s.antigravity_repair_pending()?);
        let cnt: i64 = s.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(cnt, 3);
    }
    Ok(())
}

#[test]
fn p1_v2_disabled_partial_7d76bf1_original_and_equal_length_rewrite_stay_legacy() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();

    // Test BOTH cases for 7d76bf1 partial 100 (Dec 1) overwriting Nov 29 300+20:
    // 1. Original unmodified log (reconstructed = 420, DB only saved 100 -> lacks evidence for 300+20)
    // 2. Equal-length in-place rewrite changing 300 -> 900 (reconstructed = 1020, identical DB/inode/len)
    for rewritten in [false, true] {
        let dir = tempdir()?;
        let db_v2 = dir.path().join("partial_v2.sqlite");
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-rw/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let t1 = logs.join("transcript.jsonl");

        let l1_orig = json!({
            "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 300, "output_tokens": 20
        })
        .to_string()
            + "\n";
        let l1_rewritten = json!({
            "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 900, "output_tokens": 20
        })
        .to_string()
            + "\n";
        assert_eq!(l1_orig.len(), l1_rewritten.len());

        let l2 = json!({
            "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-12-01T10:00:00+08:00",
            "cache_read_tokens": 100
        })
        .to_string()
            + "\n";

        fs::write(&t1, format!("{l1_orig}{l2}"))?;
        let fp_orig = test_file_id(&t1)?;
        let len_orig = l1_orig.len() + l2.len();

        {
            let con = rusqlite::Connection::open(&db_v2)?;
            con.execute_batch(
                "PRAGMA user_version=2;
                 CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
                 CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
                 CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
                 CREATE INDEX usage_date ON usage(provider,at);
                 CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
            )?;
            let settings = json!({
                "agents": [{"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}],
                "enabled": [],
                "confirmed": true
            });
            con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
            let dec1 = chrono::DateTime::parse_from_rfc3339("2026-12-01T10:00:00+08:00")?;
            con.execute(
                "INSERT INTO usage VALUES('antigravity','conv-rw','2',?1,100,?2,0,NULL,100,NULL,0)",
                rusqlite::params![dec1.timestamp(), dec1.timestamp_nanos_opt()],
            )?;
            con.execute(
                "INSERT INTO cursors VALUES('antigravity', ?1, ?2, ?3)",
                rusqlite::params![t1.to_string_lossy(), fp_orig, len_orig as i64],
            )?;
        }

        if rewritten {
            let mut f = fs::OpenOptions::new().write(true).truncate(true).open(&t1)?;
            f.write_all(format!("{l1_rewritten}{l2}").as_bytes())?;
            f.flush()?;
            assert_eq!(test_file_id(&t1)?, fp_orig);
            assert_eq!(t1.metadata()?.len() as usize, len_orig);
        }

        let store = Store::open(&db_v2)?;
        usage::repair_antigravity_history(&store, &brain)?;
        assert!(!store.antigravity_repair_pending()?);
        assert_eq!(store.antigravity_repair_version()?, 5);

        // Neither original 420 nor rewritten 1020 may enter verified_tokens without DB evidence of input/output!
        let con = store.connect()?;
        let rows: Vec<(String, i64, i64)> = {
            let mut stmt = con.prepare(
                "SELECT response, tokens, legacy FROM usage WHERE provider='antigravity' ORDER BY response",
            )?;
            let out = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out
        };
        assert_eq!(rows, vec![("2".to_string(), 100, 1)]);

        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let dec = history::query_with_tz(&store, Granularity::Day, 0, Some("antigravity"), None, None, today, &tz)?;
        assert_eq!(dec.summary.verified_tokens, None);
        assert_eq!((dec.summary.legacy_steps, dec.summary.legacy_tokens), (1, Some(100)));
    }
    Ok(())
}

#[test]
fn p2_mixed_7d76bf1_and_7b1ecd9_same_session_recovers_verified_step() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let db_path = dir.path().join("mixed_v3.sqlite");
    let brain = dir.path().join("brain");
    let logs = brain.join("conv-mixed/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let t1 = logs.join("transcript.jsonl");

    // Same log file holds Step 1 (300+20 then 100) + Step 2 (300+20 then 100) + post-disable Step 3 (999)
    fs::write(
        &t1,
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T09:00:00+08:00", "cache_read_tokens": 100}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T10:00:00+08:00", "input_tokens": 900, "cache_read_tokens": 49, "output_tokens": 50}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;

    // DB had Step 1 written by 7d76bf1 (Dec 1 partial 100), Step 2 written by 7b1ecd9 (Nov 29 complete 420), then 969f12a v3 cursorless.
    {
        let con = rusqlite::Connection::open(&db_path)?;
        con.execute_batch(
            "PRAGMA user_version=3;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE usage(
                provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL,
                at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER,
                cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0,
                input_at_ns INTEGER, cache_read_at_ns INTEGER, output_at_ns INTEGER,
                PRIMARY KEY(provider,session,response)
             );
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [{"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}],
            "enabled": [],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        con.execute("INSERT INTO meta VALUES('antigravity_repair_version', '2')", [])?;

        let dec1 = chrono::DateTime::parse_from_rfc3339("2026-12-01T09:00:00+08:00")?;
        let nov29 = chrono::DateTime::parse_from_rfc3339("2026-11-29T10:00:00+08:00")?;
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
             VALUES('antigravity','conv-mixed','1',?1,100,?2,0,NULL,100,NULL,1)",
            rusqlite::params![dec1.timestamp(), dec1.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,input_tokens,cache_read_tokens,output_tokens,legacy)
             VALUES('antigravity','conv-mixed','2',?1,420,?2,1,300,100,20,1)",
            rusqlite::params![nov29.timestamp(), nov29.timestamp_nanos_opt()],
        )?;
    }

    let store = Store::open(&db_path)?;
    usage::repair_antigravity_history(&store, &brain)?;
    assert!(!store.antigravity_repair_pending()?);
    assert_eq!(store.antigravity_repair_version()?, 5);

    let con = store.connect()?;
    let rows: Vec<(String, i64, i64, i64)> = {
        let mut stmt = con.prepare(
            "SELECT response, tokens, complete, legacy FROM usage WHERE provider='antigravity' ORDER BY response",
        )?;
        let out = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out
    };
    assert_eq!(rows, vec![("1".to_string(), 100, 0, 1), ("2".to_string(), 420, 1, 0)]);

    let today = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
    let nov = history::query_with_tz(&store, Granularity::Day, 3, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!(nov.summary.verified_tokens, Some(420));
    let dec1 = history::query_with_tz(&store, Granularity::Day, 1, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!((dec1.summary.verified_tokens, dec1.summary.legacy_tokens), (None, Some(100)));
    Ok(())
}

#[cfg(unix)]
#[test]
fn p2_all_unreadable_partial_readable_and_recovery_status() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("status.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs_a = brain.join("conv-a/.system_generated/logs");
    fs::create_dir_all(&logs_a)?;
    let ta = logs_a.join("transcript.jsonl");
    let today_str = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
    fs::write(
        &ta,
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": format!("{today_str}T09:00:00+08:00"), "input_tokens": 60, "cache_read_tokens": 20, "output_tokens": 20})
        ),
    )?;

    // 1. All existing logs unreadable -> Monitor::update returns Err (never Ok(Active)!)
    let orig_a = fs::metadata(&ta)?.permissions();
    fs::set_permissions(&ta, fs::Permissions::from_mode(0o000))?;
    let mut mon = Monitor::default();
    let err_all = mon.update(&store, "antigravity", &brain).unwrap_err().to_string();
    assert!(err_all.contains("不可读"), "got: {err_all}");

    // 2. Add a second readable session Z -> status = Partial, today = 222, unreadable_files = 1, warning = Some(...)
    let logs_z = brain.join("conv-z/.system_generated/logs");
    fs::create_dir_all(&logs_z)?;
    let tz_file = logs_z.join("transcript.jsonl");
    fs::write(
        &tz_file,
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": format!("{today_str}T10:00:00+08:00"), "input_tokens": 200, "cache_read_tokens": 12, "output_tokens": 10})
        ),
    )?;
    let u_partial = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_partial.status, SourceStatus::Partial);
    assert_eq!(u_partial.today, Some(222));
    assert_eq!(u_partial.unreadable_files, 1);
    assert!(u_partial.warning.as_deref().is_some_and(|w| w.contains("不可读")));

    // Appending +50 to Z while A stays unreadable still collects Z cleanly and preserves Partial
    {
        let mut f = fs::OpenOptions::new().append(true).open(&tz_file)?;
        writeln!(
            f,
            "{}",
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": format!("{today_str}T11:00:00+08:00"), "input_tokens": 30, "cache_read_tokens": 10, "output_tokens": 10})
        )?;
    }
    let u_partial2 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_partial2.status, SourceStatus::Partial);
    assert_eq!(u_partial2.today, Some(272));

    // 3. Restore A permission -> catches up A (+100 = 372), clears warning, status = Active
    fs::set_permissions(&ta, orig_a)?;
    let u_active = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_active.status, SourceStatus::Active);
    assert_eq!(u_active.today, Some(372));
    assert_eq!(u_active.unreadable_files, 0);
    assert!(u_active.warning.is_none());

    // 4. DB write error (e.g. read-only database file) propagates Err instead of being swallowed
    let db_path = dir.path().join("ro.sqlite");
    {
        let s_ro = Store::open(&db_path)?;
        s_ro.settings(false, || vec![agent("antigravity", "installed")])?;
        s_ro.select(vec!["antigravity".into()])?;
    }
    let ro_perms = fs::metadata(&db_path)?.permissions();
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o444))?;
    let s_ro = Store::open(&db_path)?;
    let db_err = Monitor::default().update(&s_ro, "antigravity", &brain);
    fs::set_permissions(&db_path, ro_perms)?;
    assert!(db_err.is_err(), "readonly sqlite must propagate write error");
    Ok(())
}

#[test]
fn history_single_period_views_exact_totals_paging_filter_and_cross_year() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("single_period.sqlite"))?;
    store.settings(false, || {
        vec![agent("codex", "installed"), agent("claude", "installed")]
    })?;
    store.select(vec!["codex".into(), "claude".into()])?;

    // Exact acceptance dataset:
    // 2026-09-28 = 10 (codex), 2026-11-01 = 30 (claude), 2026-11-23 = 50 (codex),
    // 2026-11-27 = 70 (codex), future 2026-11-28 = 90 (codex).
    for (prov, id, ts, tok) in [
        ("codex", "d_sep28", "2026-09-28T12:00:00+08:00", 10_i64),
        ("claude", "d_nov01", "2026-11-01T12:00:00+08:00", 30_i64),
        ("codex", "d_nov23", "2026-11-23T12:00:00+08:00", 50_i64),
        ("codex", "d_nov27", "2026-11-27T12:00:00+08:00", 70_i64),
        ("codex", "d_nov28_future", "2026-11-28T12:00:00+08:00", 90_i64),
    ] {
        let dt = chrono::DateTime::parse_from_rfc3339(ts)?;
        store.connect()?.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
             VALUES(?1,'s1',?2,?3,?4,?5,1,0)",
            rusqlite::params![prov, id, dt.timestamp(), tok, dt.timestamp_nanos_opt()],
        )?;
    }

    let today = NaiveDate::from_ymd_opt(2026, 11, 27).unwrap();

    // Default open on 2026-11-27:
    let day0 = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    assert_eq!((day0.range_start.as_str(), day0.range_end.as_str(), day0.summary.verified_tokens), ("2026-11-27", "2026-11-27", Some(70)));
    assert!(day0.has_older && !day0.has_newer && day0.is_current);

    let week0 = history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!((week0.range_start.as_str(), week0.range_end.as_str(), week0.summary.verified_tokens), ("2026-11-23", "2026-11-27", Some(120)));
    assert!(week0.has_older && !week0.has_newer && week0.is_current);

    let month0 = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!((month0.range_start.as_str(), month0.range_end.as_str(), month0.summary.verified_tokens), ("2026-11-01", "2026-11-27", Some(150)));
    assert!(month0.has_older && !month0.has_newer && month0.is_current);

    let year0 = history::query_with_tz(&store, Granularity::Year, 0, None, None, None, today, &tz)?;
    assert_eq!((year0.range_start.as_str(), year0.range_end.as_str(), year0.summary.verified_tokens), ("2026-01-01", "2026-11-27", Some(160)));
    assert_eq!(year0.earliest_date.as_deref(), Some("2026-09-28"));
    assert!(!year0.has_older && !year0.has_newer && year0.is_current);

    // Clicking "older" once in each view:
    let day1 = history::query_with_tz(&store, Granularity::Day, 1, None, None, None, today, &tz)?;
    assert_eq!((day1.range_start.as_str(), day1.range_end.as_str(), day1.summary.verified_tokens), ("2026-11-26", "2026-11-26", None));
    assert!(day1.has_older && day1.has_newer && !day1.is_current);

    let week1 = history::query_with_tz(&store, Granularity::Week, 1, None, None, None, today, &tz)?;
    assert_eq!((week1.range_start.as_str(), week1.range_end.as_str(), week1.summary.verified_tokens), ("2026-11-16", "2026-11-22", None));
    assert!(week1.has_older && week1.has_newer);

    let month1 = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
    assert_eq!((month1.range_start.as_str(), month1.range_end.as_str(), month1.summary.verified_tokens), ("2026-10-01", "2026-10-31", None));
    // Empty October still allows paging back to September (earliest = 2026-09-28).
    assert!(month1.has_older && month1.has_newer);
    let month2 = history::query_with_tz(&store, Granularity::Month, 2, None, None, None, today, &tz)?;
    assert_eq!((month2.range_start.as_str(), month2.range_end.as_str(), month2.summary.verified_tokens), ("2026-09-01", "2026-09-30", Some(10)));
    assert!(!month2.has_older && month2.has_newer);

    // Provider filter: Claude only has Nov 01 (30), earliest_date = 2026-11-01, Month offset=0 has has_older = false.
    let claude_month = history::query_with_tz(&store, Granularity::Month, 0, Some("claude"), None, None, today, &tz)?;
    assert_eq!(claude_month.summary.verified_tokens, Some(30));
    assert_eq!(claude_month.earliest_date.as_deref(), Some("2026-11-01"));
    assert!(!claude_month.has_older);

    // Add 2025-12-31 record -> Year enables "上一年度" and shows full 2025-01-01..2025-12-31
    let y25 = chrono::DateTime::parse_from_rfc3339("2025-12-31T23:59:59+08:00")?;
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('codex','s1','y25',?1,?2,?3,1,0)",
        rusqlite::params![y25.timestamp(), 40_i64, y25.timestamp_nanos_opt()],
    )?;
    let year0_b = history::query_with_tz(&store, Granularity::Year, 0, None, None, None, today, &tz)?;
    assert!(year0_b.has_older);
    let year1 = history::query_with_tz(&store, Granularity::Year, 1, None, None, None, today, &tz)?;
    assert_eq!((year1.range_start.as_str(), year1.range_end.as_str(), year1.summary.verified_tokens), ("2025-01-01", "2025-12-31", Some(40)));
    assert!(!year1.has_older && year1.has_newer && !year1.is_current);
    Ok(())
}

#[test]
fn p1_v3_cursorless_reenable_keeps_unverified_legacy_and_imports_new_steps() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    use std::io::Write;
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();

    for rewritten in [false, true] {
        let dir = tempdir()?;
        let db_v3 = dir.path().join("cursorless_v3.sqlite");
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-reenable/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let t1 = logs.join("transcript.jsonl");

        let l1 = json!({
            "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": if rewritten { 900 } else { 300 },
            "output_tokens": 20
        });
        let l2 = json!({
            "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-12-01T10:00:00+08:00",
            "cache_read_tokens": 100
        });
        let raw_orig = format!("{}\n{}\n", l1.to_string().replace("900", "300"), l2);
        let raw = format!("{l1}\n{l2}\n");
        if rewritten {
            assert_eq!(raw.len(), raw_orig.len());
        }
        fs::write(&t1, &raw)?;

        // Seed cursorless v3 DB left after 7d76bf1 (Dec 1 partial 100) + 969f12a (legacy=1, no cursors).
        {
            let con = rusqlite::Connection::open(&db_v3)?;
            con.execute_batch(
                "PRAGMA user_version=3;
                 CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
                 CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
                 CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
                 CREATE INDEX usage_date ON usage(provider,at);
                 CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
            )?;
            let settings = json!({
                "agents": [{"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}],
                "enabled": [],
                "confirmed": true
            });
            con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
            let dec01 = chrono::DateTime::parse_from_rfc3339("2026-12-01T10:00:00+08:00")?;
            con.execute(
                "INSERT INTO usage VALUES('antigravity','conv-reenable','2',?1,100,?2,0,NULL,100,NULL,1)",
                rusqlite::params![dec01.timestamp(), dec01.timestamp_nanos_opt()],
            )?;
        }

        // Upgrade & repair while disabled -> Step 2 must remain unverified legacy=1 (tokens=100, complete=0).
        let store = Store::open(&db_v3)?;
        usage::repair_antigravity_history(&store, &brain)?;
        let row_dump = |s: &Store| -> anyhow::Result<Vec<(String, i64, i64, i64)>> {
            let con = s.connect()?;
            let mut stmt = con.prepare(
                "SELECT response, tokens, complete, legacy FROM usage WHERE provider='antigravity' ORDER BY response",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };
        assert_eq!(row_dump(&store)?, vec![("2".into(), 100, 0, 1)]);

        // Re-enable Antigravity without appending any logs; run same Monitor x2 and fresh Monitor + reopened Store.
        store.select(vec!["antigravity".into()])?;
        let mut mon = Monitor::default();
        mon.update(&store, "antigravity", &brain)?;
        assert_eq!(row_dump(&store)?, vec![("2".into(), 100, 0, 1)]);
        mon.update(&store, "antigravity", &brain)?;
        assert_eq!(row_dump(&store)?, vec![("2".into(), 100, 0, 1)]);

        let store_reopen = Store::open(&db_v3)?;
        Monitor::default().update(&store_reopen, "antigravity", &brain)?;
        assert_eq!(row_dump(&store_reopen)?, vec![("2".into(), 100, 0, 1)]);

        // Append genuine new Step 3 (500 tokens on Dec 2): Step 2 remains legacy 100 while Step 3 is imported once.
        {
            let mut f = fs::OpenOptions::new().append(true).open(&t1)?;
            writeln!(
                f,
                "{}",
                json!({
                    "step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-12-02T10:00:00+08:00",
                    "input_tokens": 400, "cache_read_tokens": 50, "output_tokens": 50
                })
            )?;
        }
        mon.update(&store_reopen, "antigravity", &brain)?;
        mon.update(&store_reopen, "antigravity", &brain)?;
        assert_eq!(
            row_dump(&store_reopen)?,
            vec![("2".into(), 100, 0, 1), ("3".into(), 500, 1, 0)]
        );
        let today = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
        let dec = history::query_with_tz(&store_reopen, Granularity::Month, 0, Some("antigravity"), None, None, today, &tz)?;
        assert_eq!(dec.summary.verified_tokens, Some(500));
        assert_eq!(dec.summary.legacy_tokens, Some(100));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn p2_revoke_permission_after_eof_fails_partial_and_recovers_without_reread() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("revoke_eof.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs1 = brain.join("conv-one/.system_generated/logs");
    let logs2 = brain.join("conv-two/.system_generated/logs");
    fs::create_dir_all(&logs1)?;
    let t1 = logs1.join("transcript.jsonl");
    let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
    fs::write(
        &t1,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": format!("{today}T09:00:00+08:00"),
                "input_tokens": 300, "cache_read_tokens": 100, "output_tokens": 20
            })
        ),
    )?;

    // 1. Read unique transcript to EOF -> Active (420).
    let mut mon = Monitor::default();
    let u0 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u0.status, SourceStatus::Active);
    assert_eq!(u0.today, Some(420));

    // 2. Revoke permission AFTER EOF with no append -> existing Monitor AND fresh Monitor both fail!
    let orig_perm = fs::metadata(&t1)?.permissions();
    fs::set_permissions(&t1, fs::Permissions::from_mode(0o000))?;
    assert!(mon.update(&store, "antigravity", &brain).is_err());
    assert!(mon.update(&store, "antigravity", &brain).is_err());
    assert!(Monitor::default().update(&store, "antigravity", &brain).is_err());

    // 3. Restore permission -> back to Active with same 420 and still 1 row.
    fs::set_permissions(&t1, orig_perm.clone())?;
    let u_rec = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_rec.status, SourceStatus::Active);
    assert_eq!(u_rec.today, Some(420));
    assert_eq!(u_rec.unreadable_files, 0);

    // 4. Add second transcript -> total 600, then revoke only t1 after EOF -> Partial (unreadable=1, today=600).
    fs::create_dir_all(&logs2)?;
    let t2 = logs2.join("transcript.jsonl");
    fs::write(
        &t2,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": format!("{today}T10:00:00+08:00"),
                "input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 30
            })
        ),
    )?;
    let u_both = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_both.status, SourceStatus::Active);
    assert_eq!(u_both.today, Some(600));

    fs::set_permissions(&t1, fs::Permissions::from_mode(0o000))?;
    let u_part1 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_part1.status, SourceStatus::Partial);
    assert_eq!(u_part1.unreadable_files, 1);
    assert!(u_part1.warning.is_some());
    assert_eq!(u_part1.today, Some(600));

    let u_part_fresh = Monitor::default().update(&store, "antigravity", &brain)?;
    assert_eq!(u_part_fresh.status, SourceStatus::Partial);
    assert_eq!(u_part_fresh.unreadable_files, 1);

    // 5. Restore permission on t1 -> returns Active with 600 and exactly 2 rows (no duplicates).
    fs::set_permissions(&t1, orig_perm)?;
    let u_final = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u_final.status, SourceStatus::Active);
    assert_eq!(u_final.unreadable_files, 0);
    assert_eq!(u_final.warning, None);
    assert_eq!(u_final.today, Some(600));
    let rows: i64 = store
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(rows, 2);
    Ok(())
}

#[test]
fn v5_to_v6_migrates_legacy_codex_quota_and_isolates_per_platform_and_account() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("quota_v5.sqlite");
    {
        let con = rusqlite::Connection::open(&db_path)?;
        con.execute_batch(
            "PRAGMA user_version=5;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, input_at_ns INTEGER, cache_read_at_ns INTEGER, output_at_ns INTEGER, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));
             CREATE TABLE antigravity_repair_tasks(kind TEXT NOT NULL, target TEXT PRIMARY KEY, fingerprint TEXT, max_pos INTEGER, done INTEGER NOT NULL DEFAULT 0);",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null},
                {"id": "claude", "name": "Claude Code", "status": "data", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        con.execute("INSERT INTO meta VALUES('antigravity_repair_version', '5')", [])?;
        let legacy_codex_quota = json!({
            "account": "c***@example.com",
            "plan": "plus",
            "checked_at": 1795800000,
            "windows": [{"pool": "codex", "minutes": 10080, "used": 18.0, "reset": 1796200000}]
        });
        con.execute("INSERT INTO last_quota VALUES(1, ?1)", [legacy_codex_quota.to_string()])?;
        let now_ts = chrono::Utc::now().timestamp();
        con.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy) VALUES('claude','s1','m1',?1,333,?2,1,0)",
            rusqlite::params![now_ts, now_ts * 1_000_000_000],
        )?;
    }

    let store = Store::open(&db_path)?;
    assert!(!store.antigravity_repair_pending()?);
    let migrated_codex = store.cached_quota_for("codex", None)?.expect("migrated codex snapshot");
    assert_eq!(migrated_codex.provider, "codex");
    assert_eq!(migrated_codex.source, "Codex CLI app-server");
    assert_eq!(migrated_codex.account, "c***@example.com");
    assert_eq!(migrated_codex.windows[0].remaining_percent, Some(82.0));
    assert!(store.cached_quota_for("antigravity", None)?.is_none());

    // Save two separate Antigravity account snapshots -> Codex & both accounts stay isolated.
    let ag1 = quota::Quota {
        provider: "antigravity".into(),
        account_id: quota::stable_account_id("antigravity", "alice@example.com"),
        account: "a***@example.com".into(),
        plan: Some("Pro".into()),
        checked_at: 1795900000,
        source: "Antigravity CLI statusLine".into(),
        source_kind: "local_snapshot".into(),
        ai_credits: None,
        windows: vec![quota::Window {
            pool: "gemini-shared".into(),
            minutes: Some(300),
            used: 20.0,
            reset: Some(1795918000),
            label: Some("Gemini Shared".into()),
            models: vec!["Gemini 4 Neon".into(), "Gemini 4 Carbon".into()],
            remaining_percent: Some(80.0),
            remaining_amount: None,
            unit: None,
            disabled: false,
        }],
    };
    let ag2 = quota::Quota {
        account_id: quota::stable_account_id("antigravity", "bob@example.com"),
        account: "b***@example.com".into(),
        checked_at: 1795900500,
        plan: Some("Ultra".into()),
        ..ag1.clone()
    };
    store.save_quota_for("antigravity", &ag1)?;
    store.save_quota_for("antigravity", &ag2)?;

    let reopened = Store::open(&db_path)?;
    assert_eq!(reopened.cached_quota_for("codex", None)?.unwrap().account, "c***@example.com");
    assert_eq!(reopened.list_quota_snapshots("antigravity")?.len(), 2);
    assert_eq!(reopened.cached_quota_for("antigravity", ag1.account_id.as_deref())?.unwrap().plan.as_deref(), Some("Pro"));
    assert_eq!(reopened.cached_quota_for("antigravity", ag2.account_id.as_deref())?.unwrap().plan.as_deref(), Some("Ultra"));

    // History-only disabled platform returns read-only saved_totals without enabling.
    assert!(reopened.enabled_agent("claude").is_err());
    let claude_saved = usage::saved_totals(&reopened, "claude")?;
    assert_eq!(claude_saved.today, Some(333));
    Ok(())
}

#[test]
fn antigravity_statusline_parses_pools_models_amounts_resets_and_separates_context_credits() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let snap_path = dir.path().join("statusline.json");
    let captured = chrono::DateTime::parse_from_rfc3339("2026-11-27T12:00:00Z")?;
    let reset_iso = "2026-11-27T17:00:00Z";
    let expected_reset = chrono::DateTime::parse_from_rfc3339(reset_iso)?.timestamp();

    let payload = json!({
        "product": "antigravity",
        "email": "alice@example.com",
        "plan_tier": "Pro",
        "captured_at": "2026-11-27T12:00:00Z",
        "context_window": {
            "remaining_percentage": 85.76,
            "used_percentage": 14.24
        },
        "cost": {
            "total_usd": 0.42
        },
        "ai_credits": {
            "remaining_amount": 42,
            "unit": "Credits"
        },
        "quota": {
            "gemini-4-neon": {
                "pool_id": "gemini-shared",
                "display_name": "Gemini 模型共用配额",
                "model": "Gemini 4 Neon",
                "remaining_fraction": 0.85,
                "reset_time": reset_iso
            },
            "gemini-4-carbon": {
                "pool_id": "gemini-shared",
                "model": "Gemini 4 Carbon",
                "remaining_fraction": 0.85,
                "reset_time": reset_iso
            },
            "partner-credits": {
                "label": "Partner Quota",
                "remaining_amount": 120,
                "unit": "次",
                "reset_in_seconds": 3600
            },
            "legacy-pool": {
                "remaining_fraction": 0.0,
                "disabled": true
            }
        }
    });
    fs::write(&snap_path, payload.to_string())?;

    let q1 = quota::read_antigravity_statusline(&snap_path)?;
    assert_eq!(q1.provider, "antigravity");
    assert_eq!(q1.source, "Antigravity CLI statusLine");
    assert_eq!(q1.account, "a***@example.com");
    assert_eq!(q1.plan.as_deref(), Some("Pro"));
    assert_eq!(q1.checked_at, captured.timestamp());
    assert_eq!(q1.ai_credits, Some(quota::CreditBalance { amount: 42.0, unit: Some("Credits".into()) }));
    // Shared gemini-4-neon + gemini-4-carbon collapse into one pool, + partner-credits + legacy-pool = 3 rows;
    // context_window & cost never enter windows.
    assert_eq!(q1.windows.len(), 3);

    let shared = q1.windows.iter().find(|w| w.pool == "gemini-shared").unwrap();
    assert_eq!(shared.label.as_deref(), Some("Gemini 模型共用配额"));
    assert_eq!(shared.remaining_percent, Some(85.0));
    assert_eq!(shared.reset, Some(expected_reset));
    assert_eq!(shared.models, vec!["Gemini 4 Carbon", "Gemini 4 Neon"]);

    let amount_only = q1.windows.iter().find(|w| w.pool == "partner-credits").unwrap();
    assert_eq!(amount_only.remaining_percent, None);
    assert_eq!(amount_only.remaining_amount, Some(120.0));
    assert_eq!(amount_only.unit.as_deref(), Some("次"));
    assert_eq!(amount_only.reset, Some(captured.timestamp() + 3600));

    let disabled_w = q1.windows.iter().find(|w| w.pool == "legacy-pool").unwrap();
    assert!(disabled_w.disabled);
    assert_eq!(disabled_w.remaining_percent, Some(0.0));

    // Re-reading an unchanged file preserves exact checked_at (used to detect fresh=false without bumping time).
    let q2 = quota::read_antigravity_statusline(&snap_path)?;
    assert_eq!(q1, q2);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn same_initial_domain_accounts_stay_isolated_across_codex_and_antigravity_restart() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;
    let db_path = dir.path().join("accounts.sqlite");

    // Seed legacy unrecoverable snapshot ("a***@example.com", no account_id) in v5 DB first.
    {
        let con = rusqlite::Connection::open(&db_path)?;
        con.execute_batch(
            "PRAGMA user_version=5;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, input_at_ns INTEGER, cache_read_at_ns INTEGER, output_at_ns INTEGER, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));
             CREATE TABLE antigravity_repair_tasks(kind TEXT NOT NULL, target TEXT PRIMARY KEY, fingerprint TEXT, max_pos INTEGER, done INTEGER NOT NULL DEFAULT 0);",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        con.execute("INSERT INTO meta VALUES('antigravity_repair_version', '5')", [])?;
        let legacy_codex = json!({
            "account": "a***@example.com",
            "plan": "free",
            "checked_at": 1795000000,
            "windows": [{"pool": "codex", "minutes": 10080, "used": 50.0, "reset": 1795500000}]
        });
        con.execute("INSERT INTO last_quota VALUES(1, ?1)", [legacy_codex.to_string()])?;
    }

    // Build mock Codex CLI that serves alice@example.com or adam@example.com via env var.
    let bin = dir.path().join("codex-multi");
    fs::write(
        &bin,
        r#"#!/usr/bin/env python3
import json, os, sys
email = os.environ["MOCK_EMAIL"]
used = float(os.environ["MOCK_USED"])
plan = os.environ["MOCK_PLAN"]
for line in sys.stdin:
    msg = json.loads(line)
    if msg["method"] == "initialized": continue
    if msg["method"] == "initialize": result = {}
    elif msg["method"] == "account/read":
        result = {"account": {"type": "chatgpt", "email": email, "planType": plan}}
    elif msg["method"] == "account/rateLimits/read":
        result = {"rateLimitsByLimitId": {"codex": {"primary": {"windowDurationMins": 10080, "usedPercent": used, "resetsAt": 1796000000}, "secondary": None}}}
    else: sys.exit(1)
    print(json.dumps({"id": msg["id"], "result": result}), flush=True)
"#,
    )?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;

    let store = Store::open(&db_path)?;

    // Save alice@example.com & adam@example.com on Codex (both mask to "a***@example.com").
    std::env::set_var("MOCK_EMAIL", "alice@example.com");
    std::env::set_var("MOCK_USED", "10");
    std::env::set_var("MOCK_PLAN", "plus");
    let c_alice = quota::query(&bin).await?;
    std::env::set_var("MOCK_EMAIL", "adam@example.com");
    std::env::set_var("MOCK_USED", "40");
    std::env::set_var("MOCK_PLAN", "pro");
    let c_adam = quota::query(&bin).await?;

    assert_eq!(c_alice.account, "a***@example.com");
    assert_eq!(c_adam.account, "a***@example.com");
    assert_ne!(c_alice.account_id, c_adam.account_id);
    assert!(c_alice.account_id.is_some() && c_adam.account_id.is_some());
    store.save_quota_for("codex", &c_alice)?;
    store.save_quota_for("codex", &c_adam)?;

    // Save alice@example.com & adam@example.com on Antigravity via statusLine JSON.
    let ag_alice = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "alice@example.com",
            "plan_tier": "Pro",
            "captured_at": 1795900100,
            "quota": {"gemini-5h": {"remaining_fraction": 0.90}}
        }),
        1795900100,
    )?;
    let ag_adam = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "adam@example.com",
            "plan_tier": "Ultra",
            "captured_at": 1795900200,
            "quota": {"gemini-5h": {"remaining_fraction": 0.35}}
        }),
        1795900200,
    )?;
    assert_eq!(ag_alice.account, "a***@example.com");
    assert_eq!(ag_adam.account, "a***@example.com");
    assert_ne!(ag_alice.account_id, ag_adam.account_id);
    store.save_quota_for("antigravity", &ag_alice)?;
    store.save_quota_for("antigravity", &ag_adam)?;

    // Reopen Store (restart): legacy unrecoverable Codex row + alice + adam = 3 Codex rows, and 2 Antigravity rows!
    let reopened = Store::open(&db_path)?;
    let codex_all = reopened.list_quota_snapshots("codex")?;
    assert_eq!(codex_all.len(), 3, "legacy + alice + adam must all stay preserved");
    let got_c_alice = reopened.cached_quota_for("codex", c_alice.account_id.as_deref())?.unwrap();
    let got_c_adam = reopened.cached_quota_for("codex", c_adam.account_id.as_deref())?.unwrap();
    let got_c_legacy = reopened.cached_quota_for("codex", Some("a***@example.com"))?.unwrap();
    assert_eq!((got_c_alice.plan.as_deref(), got_c_alice.windows[0].remaining_percent), (Some("plus"), Some(90.0)));
    assert_eq!((got_c_adam.plan.as_deref(), got_c_adam.windows[0].remaining_percent), (Some("pro"), Some(60.0)));
    assert_eq!((got_c_legacy.plan.as_deref(), got_c_legacy.windows[0].remaining_percent), (Some("free"), Some(50.0)));

    let ag_all = reopened.list_quota_snapshots("antigravity")?;
    assert_eq!(ag_all.len(), 2, "alice and adam on Antigravity must stay separate");
    let got_a_alice = reopened.cached_quota_for("antigravity", ag_alice.account_id.as_deref())?.unwrap();
    let got_a_adam = reopened.cached_quota_for("antigravity", ag_adam.account_id.as_deref())?.unwrap();
    assert_eq!((got_a_alice.plan.as_deref(), got_a_alice.windows[0].remaining_percent), (Some("Pro"), Some(90.0)));
    assert_eq!((got_a_adam.plan.as_deref(), got_a_adam.windows[0].remaining_percent), (Some("Ultra"), Some(35.0)));
    Ok(())
}

#[test]
fn disabled_only_bucket_stays_none_and_older_or_empty_snapshot_cannot_overwrite_newer() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("merge_guards.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    // 0. First-ever anonymous empty `{}` snapshot must not insert a phantom row and returns None with guidance.
    let first_empty = quota::parse_antigravity_statusline_value(&json!({}), 150)?;
    let (eff0, upd0, note0) = store.merge_quota_for("antigravity", &first_empty)?;
    assert!(!upd0 && eff0.is_none());
    assert_eq!(note0.as_deref(), Some("暂无有效额度快照"));
    assert!(store.cached_quota_for("antigravity", None)?.is_none());
    assert!(store.list_quota_snapshots("antigravity")?.is_empty());

    // 1. Disabled-only & reset-only buckets must remain remaining_percent = None after parse, save, and reload.
    let parsed_disabled = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "alice@example.com",
            "plan_tier": "Pro",
            "captured_at": 200,
            "quota": {
                "disabled-only": {"disabled": true},
                "reset-only": {"reset_time": "2026-11-27T18:00:00Z"},
                "amount-only": {"remaining_amount": 75, "unit": "次"}
            }
        }),
        200,
    )?;
    assert_eq!(parsed_disabled.source_kind, "local_snapshot");
    let (saved, updated, _) = store.merge_quota_for("antigravity", &parsed_disabled)?;
    let saved = saved.expect("valid snapshot must be stored");
    assert!(updated);
    let reloaded = store
        .cached_quota_for("antigravity", saved.account_id.as_deref())?
        .unwrap();
    let d_w = reloaded.windows.iter().find(|w| w.pool == "disabled-only").unwrap();
    assert!(d_w.disabled && d_w.remaining_percent.is_none() && d_w.remaining_amount.is_none());
    let r_w = reloaded.windows.iter().find(|w| w.pool == "reset-only").unwrap();
    assert!(r_w.remaining_percent.is_none() && r_w.reset.is_some());
    let a_w = reloaded.windows.iter().find(|w| w.pool == "amount-only").unwrap();
    assert!(a_w.remaining_percent.is_none() && a_w.remaining_amount == Some(75.0) && a_w.unit.as_deref() == Some("次"));

    // 1b. AI Credits-only snapshot (no quota windows) is still valid effective data.
    let credits_only = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "credits@example.com",
            "captured_at": 250,
            "ai_credits": {"remaining": 42, "unit": "Credits"}
        }),
        250,
    )?;
    let (eff_c, upd_c, _) = store.merge_quota_for("antigravity", &credits_only)?;
    let eff_c = eff_c.expect("ai_credits-only snapshot must be stored");
    assert!(upd_c && eff_c.ai_credits.is_some() && eff_c.windows.is_empty());

    // 2. Save t=300 with 37%, then try importing older t=100 (95%), same-account empty t=400,
    //    AND anonymous `{}` (no email, no quota, no ai_credits) at t=400 -> must keep t=300 (37%) across refresh & reopen!
    let newer = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "alice@example.com",
            "captured_at": 300,
            "quota": {"gemini-5h": {"remaining_fraction": 0.37}}
        }),
        300,
    )?;
    let (eff1, upd1, _) = store.merge_quota_for("antigravity", &newer)?;
    let eff1 = eff1.unwrap();
    assert!(upd1 && eff1.checked_at == 300 && eff1.windows[0].remaining_percent == Some(37.0));

    let older = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "alice@example.com",
            "captured_at": 100,
            "quota": {"gemini-5h": {"remaining_fraction": 0.95}}
        }),
        100,
    )?;
    let (eff2, upd2, note2) = store.merge_quota_for("antigravity", &older)?;
    let eff2 = eff2.unwrap();
    assert!(!upd2 && note2.is_some());
    assert_eq!(eff2.checked_at, 300);
    assert_eq!(eff2.windows[0].remaining_percent, Some(37.0));

    let empty_newer = quota::parse_antigravity_statusline_value(
        &json!({
            "product": "antigravity",
            "email": "alice@example.com",
            "captured_at": 400,
            "quota": {}
        }),
        400,
    )?;
    let (eff3, upd3, note3) = store.merge_quota_for("antigravity", &empty_newer)?;
    let eff3 = eff3.unwrap();
    assert!(!upd3 && note3.is_some());
    assert_eq!(eff3.checked_at, 300);
    assert_eq!(eff3.windows[0].remaining_percent, Some(37.0));

    let anon_empty = quota::parse_antigravity_statusline_value(&json!({}), 400)?;
    assert_eq!(anon_empty.account, "未提供账号");
    let (eff4, upd4, note4) = store.merge_quota_for("antigravity", &anon_empty)?;
    let eff4 = eff4.unwrap();
    assert!(!upd4 && note4.is_some());
    assert_eq!(eff4.checked_at, 300);
    assert_eq!(eff4.account, "a***@example.com");
    assert_eq!(eff4.windows[0].remaining_percent, Some(37.0));

    // Confirm anonymous `{}` did NOT insert a phantom row in quota_snapshots.
    let raw_rows: i64 = store.connect()?.query_row(
        "SELECT COUNT(*) FROM quota_snapshots WHERE provider='antigravity'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(raw_rows, 2, "only alice and credits@example.com rows should exist");

    // Reopen DB -> default cached_quota_for("antigravity", None) STILL returns alice 37% (t=300)!
    let reopened = Store::open(&dir.path().join("merge_guards.sqlite"))?;
    let persisted = reopened.cached_quota_for("antigravity", None)?.unwrap();
    assert_eq!(persisted.checked_at, 300);
    assert_eq!(persisted.account, "a***@example.com");
    assert_eq!(persisted.windows[0].remaining_percent, Some(37.0));

    // 3. Even if a pre-existing empty row (t=999) already sits in SQLite from an older build,
    //    cached_quota_for and list_quota_snapshots skip it and surface valid 37% (t=300).
    let phantom = serde_json::to_string(&anon_empty)?;
    reopened.connect()?.execute(
        "INSERT OR REPLACE INTO quota_snapshots(provider, account_key, checked_at, updated_at, data)
         VALUES('antigravity', '未提供账号', 999, 999, ?1)",
        [phantom],
    )?;
    let reopened2 = Store::open(&dir.path().join("merge_guards.sqlite"))?;
    let resilient = reopened2.cached_quota_for("antigravity", None)?.unwrap();
    assert_eq!(resilient.checked_at, 300);
    assert_eq!(resilient.account, "a***@example.com");
    assert_eq!(resilient.windows[0].remaining_percent, Some(37.0));
    let listed = reopened2.list_quota_snapshots("antigravity")?;
    assert_eq!(listed.len(), 2, "pre-existing empty row must be skipped");
    Ok(())
}

#[test]
fn antigravity_v3_upgrade_enabled_imports_new_step3_written_before_step2_backfill_once() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_v3 = dir.path().join("enabled_v3.sqlite");
    let brain = dir.path().join("brain");
    let logs = brain.join("conv-order/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let t1 = logs.join("transcript.jsonl");

    // Log order:
    // 1) Old Step 2 initial (Nov 29: 300+20) + partial (Dec 1: 100)
    // 2) While app was closed, NEW Step 3 (500) was appended BEFORE a late backfill of Step 2!
    let lines = [
        json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
        json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
        json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T09:00:00+08:00", "input_tokens": 400, "cache_read_tokens": 50, "output_tokens": 50}),
        json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-02T10:00:00+08:00", "cache_read_tokens": 100}),
    ];
    fs::write(&t1, lines.map(|v| v.to_string()).join("\n") + "\n")?;

    // Seed enabled cursorless v3 DB containing Step 2 (420 complete from 7b1ecd9 -> legacy=1 in v3, no cursors).
    {
        let con = rusqlite::Connection::open(&db_v3)?;
        con.execute_batch(
            "PRAGMA user_version=3;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [{"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}],
            "enabled": ["antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        let nov29 = chrono::DateTime::parse_from_rfc3339("2026-11-29T10:00:00+08:00")?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-order','2',?1,420,?2,1,300,100,20,1)",
            rusqlite::params![nov29.timestamp(), nov29.timestamp_nanos_opt()],
        )?;
    }

    let store = Store::open(&db_v3)?;
    usage::repair_antigravity_history(&store, &brain)?;
    let mut mon = Monitor::default();
    mon.update(&store, "antigravity", &brain)?;

    let dump = |s: &Store| -> anyhow::Result<Vec<(String, i64, i64, i64)>> {
        let con = s.connect()?;
        let mut stmt = con.prepare(
            "SELECT response, tokens, complete, legacy FROM usage WHERE provider='antigravity' ORDER BY response",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    };
    assert_eq!(
        dump(&store)?,
        vec![("2".into(), 420, 1, 0), ("3".into(), 500, 1, 0)],
        "Step 3 written before Step 2 backfill must be imported once"
    );

    // Reopen twice -> zero duplicates.
    for _ in 0..2 {
        let reopened = Store::open(&db_v3)?;
        Monitor::default().update(&reopened, "antigravity", &brain)?;
        assert_eq!(dump(&reopened)?, vec![("2".into(), 420, 1, 0), ("3".into(), 500, 1, 0)]);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn codex_and_claude_unreadable_eof_file_does_not_block_sibling_file_new_tokens() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let today_rfc = chrono::Utc::now().to_rfc3339();

    // Test both Codex and Claude Code.
    for (prov, make_line) in [
        (
            "codex",
            Box::new(|id: &str, tok: i64, ts: &str| {
                json!({
                    "type": "token_usage_record",
                    "timestamp": ts,
                    "payload": {"session_id": "s1", "response_id": id, "usage": {"total_tokens": tok}}
                })
                .to_string()
            }) as Box<dyn Fn(&str, i64, &str) -> String>,
        ),
        (
            "claude",
            Box::new(|id: &str, tok: i64, ts: &str| {
                json!({
                    "type": "assistant",
                    "timestamp": ts,
                    "sessionId": "s1",
                    "message": {"id": id, "usage": {"input_tokens": tok, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
                })
                .to_string()
            }) as Box<dyn Fn(&str, i64, &str) -> String>,
        ),
    ] {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("sibling.sqlite"))?;
        store.settings(false, || vec![agent(prov, "installed")])?;
        store.select(vec![prov.into()])?;
        let root = dir.path().join("logs");
        fs::create_dir_all(&root)?;
        let f1 = root.join("01_old.jsonl");
        let f2 = root.join("02_live.jsonl");

        fs::write(&f1, format!("{}\n", make_line("r1", 300, &today_rfc)))?;
        fs::write(&f2, "")?;

        let mut mon = Monitor::default();
        let u1 = mon.update(&store, prov, &root)?;
        assert_eq!(u1.today, Some(300));
        assert_eq!(u1.status, SourceStatus::Active);

        // Revoke permission on old EOF file 01_old.jsonl, then append 300 tokens to 02_live.jsonl.
        let orig = fs::metadata(&f1)?.permissions();
        fs::set_permissions(&f1, fs::Permissions::from_mode(0o000))?;
        fs::write(&f2, format!("{}\n", make_line("r2", 300, &today_rfc)))?;

        let u2 = mon.update(&store, prov, &root)?;
        assert_eq!(u2.today, Some(600), "{prov}: sibling file must still ingest +300 -> 600");
        assert_eq!(u2.status, SourceStatus::Partial);
        assert_eq!(u2.unreadable_files, 1);

        // Fresh Monitor must also keep 600 & Partial.
        let u2_fresh = Monitor::default().update(&store, prov, &root)?;
        assert_eq!(u2_fresh.today, Some(600));
        assert_eq!(u2_fresh.status, SourceStatus::Partial);

        // Restore permission -> Active, still 600 (no duplication).
        fs::set_permissions(&f1, orig)?;
        let u3 = mon.update(&store, prov, &root)?;
        assert_eq!(u3.today, Some(600));
        assert_eq!(u3.status, SourceStatus::Active);
        assert_eq!(u3.unreadable_files, 0);
    }
    Ok(())
}

#[test]
fn db_history_only_platform_survives_rescan_and_restart_with_saved_usage() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("db_history_only.sqlite");
    let store = Store::open(&db_path)?;
    store.settings(false, || vec![agent("claude", "installed"), agent("codex", "installed")])?;
    store.select(vec!["claude".into(), "codex".into()])?;

    let now_ts = chrono::Utc::now().timestamp();
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('claude','s1','m1',?1,777,?2,1,0)",
        rusqlite::params![now_ts, now_ts * 1_000_000_000],
    )?;

    // Disable Claude, then rescan where both app and ~/.claude/projects are gone ("missing").
    store.select(vec!["codex".into()])?;
    let after_rescan = store.settings(true, || {
        vec![agent("claude", "missing"), agent("codex", "installed")]
    })?;
    let claude = after_rescan.agents.iter().find(|a| a.id == "claude").unwrap();
    assert_eq!(claude.status, "history", "DB-only platform must surface as history after rescan");
    assert!(!after_rescan.enabled.contains(&"claude".to_string()));
    assert!(store.select(vec!["codex".into(), "claude".into()]).is_err(), "DB-only platform without app/dir cannot be enabled");

    // Restart (reopen Store + settings(false)): still status="history" and saved_totals reads 777 read-only.
    let reopened = Store::open(&db_path)?;
    let after_restart = reopened.settings(false, Vec::new)?;
    assert_eq!(
        after_restart.agents.iter().find(|a| a.id == "claude").unwrap().status,
        "history"
    );
    let saved = usage::saved_totals(&reopened, "claude")?;
    assert_eq!(saved.today, Some(777));
    Ok(())
}

#[cfg(unix)]
#[test]
fn antigravity_statusline_bridge_cleans_temp_files_and_preserves_upstream_output_and_exit_code()
-> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let src_bridge = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/antigravity-statusline-bridge.sh");
    assert!(src_bridge.is_file(), "bridge script must exist at {src_bridge:?}");

    let dir = tempdir()?;
    // Put both the copied bridge script and data dir under paths containing spaces.
    let app_dir = dir.path().join("Agent Use.app/Contents/Resources");
    let data_dir = dir.path().join("Application Support/dev.agentuse.desktop");
    let tmp_dir = dir.path().join("isolated tmp");
    fs::create_dir_all(&app_dir)?;
    fs::create_dir_all(&tmp_dir)?;
    let bridge = app_dir.join("antigravity-statusline-bridge.sh");
    fs::copy(&src_bridge, &bridge)?;
    fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755))?;

    // Upstream script echoes stdin byte length & custom bar line, and exits with MOCK_EXIT_CODE.
    let upstream = dir.path().join("my statusline.sh");
    fs::write(
        &upstream,
        "#!/usr/bin/env sh\npayload=\"$(cat)\"\nprintf 'CUSTOM_BAR|%s\\n' \"$payload\"\nexit \"${MOCK_EXIT_CODE:-0}\"\n",
    )?;
    fs::set_permissions(&upstream, fs::Permissions::from_mode(0o755))?;

    let run_bridge = |payload: &str,
                      target_env: (&str, &std::path::Path),
                      exit_code: &str|
     -> anyhow::Result<(i32, String)> {
        let mut child = Command::new(&bridge)
            .arg(&upstream)
            .env("TMPDIR", &tmp_dir)
            .env(target_env.0, target_env.1)
            .env("MOCK_EXIT_CODE", exit_code)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.as_mut().unwrap().write_all(payload.as_bytes())?;
        let out = child.wait_with_output()?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    };

    // Case 1: 3 consecutive runs -> 0 leftover temp files, snapshot updated, stdout intact.
    for i in 1..=3 {
        let payload = format!(
            r#"{{"product":"antigravity","email":"alice@example.com","captured_at":{},"quota":{{"gemini-5h":{{"remaining_fraction":0.8{}}}}}}}"#,
            1796000000 + i,
            i
        );
        let (code, stdout) = run_bridge(&payload, ("AGENT_USE_DATA_DIR", &data_dir), "0")?;
        assert_eq!(code, 0);
        assert_eq!(stdout, format!("CUSTOM_BAR|{payload}\n"));
    }
    let tmp_leftovers: Vec<_> = fs::read_dir(&tmp_dir)?.collect::<Result<_, _>>()?;
    assert!(
        tmp_leftovers.is_empty(),
        "TMPDIR must have 0 leftover files after 3 runs, found: {tmp_leftovers:?}"
    );
    let snap_path = data_dir.join("antigravity_statusline.json");
    let snap = quota::read_antigravity_statusline(&snap_path)?;
    assert_eq!(snap.checked_at, 1796000003);
    assert_eq!(snap.windows[0].remaining_percent, Some(83.0));
    let hidden_leftovers: Vec<_> = fs::read_dir(&data_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(hidden_leftovers.is_empty(), "data_dir must have no leftover temp files");

    // Case 2: Unwritable target directory -> upstream still receives full stdin, outputs stdout, exits 0, 0 temp files.
    let ro_dir = dir.path().join("readonly_target");
    fs::create_dir_all(&ro_dir)?;
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555))?;
    let payload_ro = r#"{"product":"antigravity","quota":{"gemini-5h":{"remaining_fraction":0.5}}}"#;
    let res_ro = run_bridge(
        payload_ro,
        ("AGENT_USE_ANTIGRAVITY_STATUSLINE_PATH", &ro_dir.join("snap.json")),
        "0",
    );
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755))?;
    let (code_ro, stdout_ro) = res_ro?;
    assert_eq!(code_ro, 0, "unwritable target must not break upstream exit code");
    assert_eq!(stdout_ro, format!("CUSTOM_BAR|{payload_ro}\n"));
    assert!(
        fs::read_dir(&tmp_dir)?.next().is_none(),
        "TMPDIR must have 0 leftover files on unwritable target"
    );

    // Case 3: Upstream exits non-zero (7) -> preserve stdout, write snapshot, clean temp files, return 7.
    let payload_nz = r#"{"product":"antigravity","captured_at":1796000099,"quota":{"gemini-5h":{"remaining_fraction":0.42}}}"#;
    let (code_nz, stdout_nz) = run_bridge(payload_nz, ("AGENT_USE_DATA_DIR", &data_dir), "7")?;
    assert_eq!(code_nz, 7, "must propagate exact upstream non-zero exit code");
    assert_eq!(stdout_nz, format!("CUSTOM_BAR|{payload_nz}\n"));
    assert!(
        fs::read_dir(&tmp_dir)?.next().is_none(),
        "TMPDIR must have 0 leftover files on non-zero upstream exit"
    );
    let snap_nz = quota::read_antigravity_statusline(&snap_path)?;
    assert_eq!(snap_nz.checked_at, 1796000099);
    assert_eq!(snap_nz.windows[0].remaining_percent, Some(42.0));
    Ok(())
}
