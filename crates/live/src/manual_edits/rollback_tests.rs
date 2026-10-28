use super::apply::{clear_manual_apply_transaction, manual_apply_transaction_path, rollback_apply_snapshot, rollback_manual_apply_transaction};
use super::buffer::stage_entry;
use crate::server_state::FileSnapshot;
use crate::util::Env;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture() -> (PathBuf, String, Env, Value) {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("impeccable-rollback-test-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let cwd = dir.to_string_lossy().into_owned();
    let env = Env::new();
    let entry = json!({"id":"e1","pageUrl":"/","ops":[{"ref":"p","tag":"p","originalText":"Alpha","newText":"Bravo","sourceHint":{"file":"page.html","line":1}}]});
    stage_entry(&cwd, &env, &entry).unwrap();
    let batch = json!({"entries":[entry]});
    (dir, cwd, env, batch)
}

fn write_transaction(cwd: &str, env: &Env, value: &Value) -> String {
    let path = manual_apply_transaction_path(cwd, env);
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path
}

#[test]
fn failed_transaction_preserves_concurrent_file_and_transaction() {
    let (dir, cwd, env, _) = fixture();
    let source = dir.join("page.html");
    std::fs::write(&source, "Concurrent user edit").unwrap();
    let txn = json!({"version":1,"id":"txn-1","pageUrl":"/","entryIds":["e1"],"files":[{"file":"page.html","exists":true,"content":"Alpha"}]});
    let txn_path = write_transaction(&cwd, &env, &txn);
    let before = std::fs::read(&txn_path).unwrap();
    let (result, _) = rollback_manual_apply_transaction(&cwd, &env, Some("/"), "apply_failed");
    assert_eq!(std::fs::read(&source).unwrap(), b"Concurrent user edit", "rollback must not overwrite concurrent source changes");
    assert_eq!(std::fs::read(&txn_path).unwrap(), before, "unresolved transaction must remain intact");
    let result = result.expect("failure must be reported");
    assert!(result.get("recoveryRequired").and_then(Value::as_bool) == Some(true) || result.get("rollbackFailures").and_then(Value::as_array).is_some_and(|v| !v.is_empty()), "manual recovery must be explicit: {result}");
}

#[test]
fn snapshot_rollback_never_overwrites_or_deletes_concurrent_file() {
    let (dir, cwd, _, _) = fixture();
    let first = dir.join("page.html");
    let second = dir.join("created.html");
    std::fs::write(&first, "Concurrent user edit").unwrap();
    std::fs::write(&second, "User created this").unwrap();
    let batch = json!({"entries":[{"id":"e1","ops":[{"sourceHint":{"file":"page.html"}},{"sourceHint":{"file":"created.html"}}]}]});
    let snapshot = vec![("page.html".to_string(), FileSnapshot{exists:true,content:"Alpha".to_string()}),("created.html".to_string(), FileSnapshot{exists:false,content:String::new()})];
    let result = rollback_apply_snapshot(&batch, &snapshot, &[], &cwd);
    assert_eq!(std::fs::read(&first).unwrap(), b"Concurrent user edit");
    assert_eq!(std::fs::read(&second).unwrap(), b"User created this");
    assert!(result["rollbackFailures"].as_array().is_some_and(|v| !v.is_empty()), "manual recovery must be reported: {result}");
}

#[test]
fn transaction_clear_refuses_corrupt_missing_or_mismatched_identity() {
    let (_, cwd, env, _) = fixture();
    let path = manual_apply_transaction_path(&cwd, &env);
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    for (bytes, id) in [
        (b"{broken".as_slice(), Some("txn-1")),
        (br#"{"version":1}"#.as_slice(), Some("txn-1")),
        (br#"{"version":1,"id":"txn-2"}"#.as_slice(), Some("txn-1")),
        (br#"{"version":1,"id":"txn-1"}"#.as_slice(), None),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(!clear_manual_apply_transaction(&cwd, &env, id));
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "invalid or unmatched transaction must stay intact");
    }
}

#[test]
fn transaction_creation_refuses_unreadable_snapshot_and_existing_record() {
    use super::apply::write_manual_apply_transaction;
    let (dir, cwd, env, batch) = fixture();
    // A directory at the source path cannot be read as a file on Windows or Unix.
    std::fs::create_dir(dir.join("page.html")).unwrap();
    let path = manual_apply_transaction_path(&cwd, &env);
    assert!(write_manual_apply_transaction(&cwd, &env, &json!("/"), &batch).is_err());
    assert!(!std::path::Path::new(&path).exists(), "failed snapshot must not create a transaction");

    std::fs::remove_dir(dir.join("page.html")).unwrap();
    std::fs::write(dir.join("page.html"), "Alpha").unwrap();
    let original = json!({"version":1,"id":"prior","pageUrl":"/","entryIds":["e1"],"files":[]});
    write_transaction(&cwd, &env, &original);
    let before = std::fs::read(&path).unwrap();
    assert!(write_manual_apply_transaction(&cwd, &env, &json!("/"), &batch).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "existing transaction must not be replaced");
}

#[test]
fn transaction_create_child() {
    use std::io::Write;
    let Some(cwd) = std::env::var_os("IMPECCABLE_ROLLBACK_CHILD_CWD") else { return };
    let cwd = cwd.to_string_lossy();
    let batch = json!({"entries":[{"id":"e1","pageUrl":"/","ops":[{"sourceHint":{"file":"page.html"}}]}]});
    println!("TXN_CHILD_READY");
    std::io::stdout().flush().unwrap();
    let result = super::apply::write_manual_apply_transaction(&cwd, &Env::new(), &json!("/"), &batch);
    println!("TXN_CHILD_RESULT={}", if result.is_ok() { "created" } else { "refused" });
}

#[test]
fn two_processes_create_only_one_transaction() {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let (dir, cwd, env, _) = fixture();
    std::fs::write(dir.join("page.html"), "Original source").unwrap();
    let txn_path = manual_apply_transaction_path(&cwd, &env);
    let lock_path = std::path::Path::new(&txn_path).parent().unwrap().join("locks").join("manual-edit-apply-transaction.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let guard = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path).unwrap();
    guard.lock().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut children = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..2 {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "manual_edits::rollback_tests::transaction_create_child", "--nocapture"])
            .env("IMPECCABLE_ROLLBACK_CHILD_CWD", &cwd)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        impeccable_common::proc::hide_window(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let sender = ready_tx.clone();
        readers.push(std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut output = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 { break; }
                let ready = line.contains("TXN_CHILD_READY");
                output.push_str(&line);
                if ready { sender.send(()).unwrap(); break; }
            }
            reader.read_to_string(&mut output).unwrap();
            output
        }));
        children.push(child);
    }
    drop(ready_tx);
    let first_ready = ready_rx.recv_timeout(Duration::from_secs(15));
    let second_ready = ready_rx.recv_timeout(Duration::from_secs(15));
    drop(guard);
    assert!(first_ready.is_ok() && second_ready.is_ok(), "both child processes must reach the lock");
    let outputs: Vec<String> = readers.into_iter().map(|thread| thread.join().unwrap()).collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "child failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert_eq!(outputs.iter().filter(|s| s.contains("TXN_CHILD_RESULT=created")).count(), 1, "one creator: {outputs:?}");
    assert_eq!(outputs.iter().filter(|s| s.contains("TXN_CHILD_RESULT=refused")).count(), 1, "one refusal: {outputs:?}");
    let tx = super::apply::read_manual_apply_transaction(&cwd, &env).expect("valid transaction must remain");
    assert!(tx["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(tx["files"][0]["file"], "page.html");
    assert_eq!(tx["files"][0]["content"], "Original source");
}

#[test]
fn cancel_and_late_reply_preserve_dispatched_evidence() {
    use crate::server_state::{ApplyDeferred, PendingEntry, ServerState};
    use crate::session::create_live_session_store;
    use std::collections::HashMap;
    use std::sync::{mpsc::channel, Weak};

    let (dir, cwd, env, batch) = fixture();
    let source = dir.join("page.html");
    std::fs::write(&source, "Concurrent user content").unwrap();
    let evidence_path = super::apply::write_manual_apply_evidence("event1", &batch, &cwd, &env);
    let original = std::fs::read(&evidence_path).expect("evidence must be created by the fixture");
    let event = json!({"id":"event1","type":"manual_edit_apply","pageUrl":"/","batch":batch,"evidencePath":evidence_path}).as_object().unwrap().clone();
    let (log_tx, _log_rx) = channel();
    let (tx, rx) = channel();
    let mut st = ServerState {
        token:String::new(), port:0, cwd:cwd.clone(), env:env.clone(), roots:None,
        sse_clients:Vec::new(), pending_events:vec![PendingEntry {event:event.clone(),lease_until:0,seq:1}],
        pending_polls:Vec::new(), next_event_seq:1, last_agent_polling_broadcast:None,
        exit_timer_gen:0, exit_timer_active:false, session_dir:None,
        store:create_live_session_store(&cwd,&env,None), lease_timer_gen:0, lease_timer_active:false,
        manual_edit_activity:None, next_manual_edit_seq:1,
        pending_apply_deferreds:vec![("event1".to_string(),ApplyDeferred {tx,event:event.clone(),batch:batch.clone(),page_url:json!("/"),rollback_snapshot:vec![("page.html".to_string(),FileSnapshot{exists:true,content:"Earlier content".to_string()})],cwd:cwd.clone(),timer_gen:1})],
        commits_in_progress:Vec::new(), last_poll_at:0, timed_out_apply_ids:Vec::new(),
        next_poll_id:1, next_client_id:1, next_apply_timer_gen:1, pending_agent_targets:Vec::new(),
        next_agent_target_timer_gen:0, resolved_agent_targets:Vec::new(), hide_live_bar:false,
        shutting_down:false, cleaned_up:false, log_tx, debug_manual_edit_events:false,
        self_ref:Weak::new(), source_resolution_cache:HashMap::new(), detect_script:String::new(),
    };
    let canceled = super::apply::cancel_pending_events(&mut st, Some("/"), "cancelled");
    assert_eq!(canceled.len(), 1);
    assert!(rx.try_recv().unwrap().is_err(), "waiting agent receives cancellation");
    assert_eq!(std::fs::read(&source).unwrap(), b"Concurrent user content", "cancel must not overwrite concurrent source");
    assert_eq!(std::fs::read(&evidence_path).expect("dispatched evidence must survive cancellation"), original);
    let msg = json!({"id":"event1","data":{"files":["page.html"]}}).as_object().unwrap().clone();
    let _late_result = super::apply::rollback_timed_out_reply(&mut st, &msg);
    assert_eq!(std::fs::read(&source).unwrap(), b"Concurrent user content");
    assert_eq!(std::fs::read(&evidence_path).expect("dispatched evidence must survive late reply"), original);
    let transaction = json!({"version":1,"id":"txn1","pageUrl":"/","entryIds":["e1"],"files":[]});
    write_transaction(&cwd, &env, &transaction);
    assert!(super::apply::prune_stale_evidence(&st).is_empty(), "unresolved transaction must prevent pruning");
    assert_eq!(std::fs::read(&evidence_path).expect("evidence must survive pruning"), original);
}

#[test]
fn absent_transaction_recovery_and_clear_leave_no_impeccable_directory() {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("impeccable-rollback-empty-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let cwd = dir.to_string_lossy();
    let env = Env::new();
    let (result, activity) = rollback_manual_apply_transaction(&cwd, &env, Some("/"), "startup");
    assert!(result.is_none() && activity.is_none());
    assert!(!dir.join(".impeccable").exists(), "empty recovery must not create state");
    assert!(!clear_manual_apply_transaction(&cwd, &env, Some("txn-1")));
    assert!(!dir.join(".impeccable").exists(), "empty clear must not create state");
}

#[test]
fn transaction_lock_is_in_live_locks_without_legacy_sibling() {
    let (dir, cwd, env, batch) = fixture();
    std::fs::write(dir.join("page.html"), "Original").unwrap();
    let txn_path = manual_apply_transaction_path(&cwd, &env);
    super::apply::write_manual_apply_transaction(&cwd, &env, &json!("/"), &batch).unwrap();
    let live_dir = std::path::Path::new(&txn_path).parent().unwrap();
    assert_eq!(live_dir, dir.join(".impeccable").join("live"));
    assert!(live_dir.join("locks").join("manual-edit-apply-transaction.lock").is_file());
    assert!(!std::path::Path::new(&format!("{txn_path}.lock")).exists(), "legacy sibling lock must not be created");
}