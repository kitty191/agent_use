use agent_core::{
    discovery::Agent,
    quota,
    store::Store,
    usage::{Monitor, SourceStatus},
};
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

    // Brain directory appears -> auto-recovers to Active; empty directory still has None (not fake 0).
    fs::create_dir_all(&brain)?;
    let empty_active = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(empty_active.status, SourceStatus::Active);
    assert_eq!(empty_active.today, None);

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

        // Seed EOF cursors for Codex and Antigravity (using dummy fingerprints & exact file lengths).
        con.execute(
            "INSERT INTO cursors VALUES('codex', ?1, 'codex-fp', ?2)",
            rusqlite::params![codex_file.to_string_lossy(), codex_file.metadata()?.len() as i64],
        )?;
        for f in [&file_a, &file_b, &file_c] {
            con.execute(
                "INSERT INTO cursors VALUES('antigravity', ?1, 'old-fp', ?2)",
                rusqlite::params![f.to_string_lossy(), f.metadata()?.len() as i64],
            )?;
        }
    }

    // Open with new Store -> upgrades schema to v2, resets ONLY Antigravity cursors.
    let store = Store::open(&db_path)?;
    {
        let con = store.connect()?;
        let ver: i32 = con.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(ver, 2);
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
        assert_eq!(ag_cursors_before, 0, "Antigravity cursors should be cleared once for backfill");
    }

    // Run Antigravity monitor after upgrade:
    // - conv-a & conv-b recover sub-second precision -> latest = 200 (conv-b 10:00:01.900)
    // - conv-c is corrected from false-complete 320 to incomplete (today_incomplete_steps = 1)
    // - conv-deleted is preserved as legacy history (777 tokens, today_legacy_steps = 1)
    // - complete today sum = 100 + 200 + 777 = 1077 (no doubling!)
    let u_after = Monitor::default().update(&store, "antigravity", &brain)?;
    assert_eq!(u_after.latest, Some(200));
    assert_eq!(u_after.today, Some(1077));
    assert_eq!(u_after.today_complete_steps, 3);
    assert_eq!(u_after.today_incomplete_steps, 1);
    assert_eq!(u_after.today_legacy_steps, 1);
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
    assert_eq!(u_reopened.today, Some(1077));
    assert_eq!(u_reopened.today_incomplete_steps, 1);
    assert_eq!(u_reopened.today_legacy_steps, 1);

    // Codex history remained untouched (5000, legacy = 0).
    let (codex_tokens, codex_legacy): (i64, i64) = store_reopened.connect()?.query_row(
        "SELECT tokens, legacy FROM usage WHERE provider='codex'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((codex_tokens, codex_legacy), (5000, 0));

    Ok(())
}
