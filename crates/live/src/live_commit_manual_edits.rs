//! JS: live-commit-manual-edits.mjs `main()` -> `impeccable
//! live-commit-manual-edits`. Applies pending live copy edits as one AI-owned
//! batch and prints the commit result as JSON.

use crate::manual_edits::apply::{clear_manual_apply_transaction, count_manual_apply_ops, write_manual_apply_transaction};
use crate::manual_edits::commit::{commit_manual_edits_value_tracked, preflight_batch, CommitOptions};
use crate::manual_edits::evidence::build_manual_edit_evidence_value;
use crate::util::{eprintln, json_compact, println};
use impeccable_common::Io;
use serde_json::{json, Value};

/// JS: argVal(args, name). A bare flag yields the boolean `true`.
pub(crate) fn arg_val(args: &[String], name: &str) -> Value {
    let prefix = format!("{}=", name);
    for arg in args {
        if arg == name {
            return Value::Bool(true);
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Value::String(rest.to_string());
        }
    }
    Value::Null
}

// Only a definite successful result permits removing the transaction.
// Unknown or failed outcomes preserve the source snapshot for recovery.
pub(crate) fn successful_result(result: &Value, expected_count: usize) -> bool {
    let empty_or_absent = |key| match result.get(key) { None => true, Some(Value::Array(a)) => a.is_empty(), _ => false };
    result.get("applied").and_then(Value::as_array).is_some_and(|v| !v.is_empty())
        && result.get("failed").and_then(Value::as_array).is_some_and(|v| v.is_empty())
        && result.get("files").and_then(Value::as_array).is_some()
        && result.get("pageUrl").is_some()
        && result.get("count").and_then(Value::as_u64) == Some(expected_count as u64)
        && result.get("cleared").and_then(Value::as_u64) == Some(expected_count as u64)
        && result.get("error").is_none()
        && result.get("reason").is_none()
        && match result.get("status") { None => true, Some(Value::String(s)) => s == "done" || s == "ok", _ => false }
        && empty_or_absent("conflicts")
        && empty_or_absent("rollbackFailures")
        && matches!(result.get("needsManualDecision"), None | Some(Value::Bool(false)))
        && matches!(result.get("recoveryRequired"), None | Some(Value::Bool(false)))
}

// Only the known pre-provider failure shape can be marked as not needing
// transaction recovery. The dispatch flag must also independently be false.
pub(crate) fn predispatch_failed_without_mutation(result: &Value, expected_count: usize) -> bool {
    result.get("applied").and_then(Value::as_array).is_some_and(|v| v.is_empty())
        && result.get("failed").and_then(Value::as_array).is_some_and(|v| !v.is_empty())
        && result.get("files").and_then(Value::as_array).is_some_and(|v| v.is_empty())
        && result.get("rolledBackFiles").and_then(Value::as_array).is_some_and(|v| v.is_empty())
        && result.get("cleared").and_then(Value::as_u64) == Some(0)
        && result.get("count").and_then(Value::as_u64) == Some(expected_count as u64)
        && result.get("pageUrl").is_some()
        && result.get("needsManualDecision") == Some(&Value::Bool(true))
        && result.get("recoveryRequired") == Some(&Value::Bool(true))
        && result.get("rollbackFailures").and_then(Value::as_array).is_some_and(|items| !items.is_empty()
            && items.iter().all(|item| item.get("reason").and_then(Value::as_str) == Some("manual_recovery_required")))
        && ["error", "status", "reason", "transactionId", "transactionReleased", "sourceChangeNotDispatched", "sourceRollbackNotNeeded", "priorRollbackDiagnostics"]
            .iter().all(|key| result.get(key).is_none())
}

// Call only after the caller has verified dispatch did not begin and has
// cleared the matching transaction. Preserve the old diagnostic evidence but
// do not report a failed rollback that was never needed.
pub(crate) fn mark_predispatch_transaction_released(result: &mut Value, id: &str, expected_count: usize) -> bool {
    if id.is_empty() || !predispatch_failed_without_mutation(result, expected_count) { return false; }
    let Some(object) = result.as_object_mut() else { return false; };
    let Some(prior) = object.remove("rollbackFailures") else { return false; };
    object.insert("priorRollbackDiagnostics".into(), prior);
    object.insert("rollbackFailures".into(), json!([]));
    object.insert("transactionId".into(), json!(id));
    object.insert("transactionReleased".into(), Value::Bool(true));
    object.insert("sourceChangeNotDispatched".into(), Value::Bool(true));
    object.insert("sourceRollbackNotNeeded".into(), Value::Bool(true));
    object.insert("recoveryRequired".into(), Value::Bool(false));
    true
}

// The callbacks make ordering and cleanup decisions testable without file IO.
fn coordinate_batch<P, T, C, A>(batch: &Value, mut preflight: P, create: T, mut clear: C, apply: A) -> Result<Value, String>
where
    P: FnMut(&Value) -> Result<(), String>,
    T: FnOnce(&Value) -> Result<String, String>,
    C: FnMut(&str) -> bool,
    A: FnOnce(&Value, Option<&str>) -> (Result<Value, String>, bool),
{
    if count_manual_apply_ops(batch) == 0 {
        return apply(batch, None).0;
    }
    preflight(batch)?;
    let id = create(batch)?;
    if id.is_empty() {
        return Err("manual_edit_transaction_invalid_id".into());
    }
    // A competing discard or stage can occur between the first preflight and
    // transaction creation. No agent has been invoked at this point.
    if let Err(error) = preflight(batch) {
        return if clear(&id) { Err(error) } else {
            Err(format!("{error}; manual_edit_transaction_clear_failed: {id}"))
        };
    }
    // The tracked flag is set immediately before provider dispatch; never
    // infer whether an agent ran from error strings or result content.
    let (result, dispatched) = apply(batch, Some(&id));
    let mut result = match result {
        Ok(value) => value,
        Err(error) if !dispatched => return if clear(&id) { Err(error) } else {
            Err(format!("{error}; manual_edit_transaction_clear_failed: {id}"))
        },
        Err(error) => return Err(format!("manual_edit_recovery_required: transaction {id}: {error}")),
    };
    if successful_result(&result, count_manual_apply_ops(batch)) {
        if !clear(&id) {
            return Err(format!("manual_edit_transaction_clear_failed_after_apply: {id}"));
        }
    } else if !dispatched && predispatch_failed_without_mutation(&result, count_manual_apply_ops(batch)) {
        if !clear(&id) {
            return Err(format!("manual_edit_transaction_clear_failed_before_dispatch: {id}"));
        }
        if !mark_predispatch_transaction_released(&mut result, &id, count_manual_apply_ops(batch)) {
            return Err(format!("manual_edit_released_transaction_result_invalid: {id}"));
        }
    } else if let Some(object) = result.as_object_mut() {
        if let Some(previous) = object.get("transactionId").filter(|v| v.as_str() != Some(&id)).cloned() {
            object.entry("reportedTransactionId").or_insert(previous);
        }
        object.insert("transactionId".into(), json!(id));
        object.insert("recoveryRequired".into(), Value::Bool(true));
        object.entry("reason").or_insert(json!("manual_edit_recovery_required"));
    } else {
        return Err(format!("manual_edit_recovery_required: transaction {id}: unrecognized_apply_result"));
    }
    Ok(result)
}

pub fn run(args: &[String], io: &mut Io) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println(
            io,
            "Usage: impeccable live-commit-manual-edits [--page-url=<url>] [--provider=auto|codex|claude|mock]",
        );
        return 0;
    }

    let cwd = io.cwd.to_string_lossy().into_owned();
    let env = io.env.clone();
    let page_url = arg_val(args, "--page-url");
    // JS-PARITY: live-commit-manual-edits.mjs#main passes `argVal(...) ||
    // undefined`, so a bare `--provider` reaches the runner as the boolean
    // `true` and falls through to "Unsupported live copy-edit AI runner: true".
    // The string "true" reproduces every branch of that path.
    let provider: Option<String> = match arg_val(args, "--provider") {
        Value::Bool(true) => Some("true".to_string()),
        Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    };
    let timeout_ms = match env.get("IMPECCABLE_LIVE_COPY_AGENT_TIMEOUT_MS") {
        Some(v) if !v.is_empty() => impeccable_core::js::string_to_number(v),
        _ => 120_000.0,
    };

    let result = (|| {
        let batch = build_manual_edit_evidence_value(&cwd, &env, &page_url)?;
        coordinate_batch(
            &batch,
            |batch| preflight_batch(&cwd, &env, batch),
            |batch| {
                let tx = write_manual_apply_transaction(&cwd, &env, &page_url, batch)?;
                tx.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())
                    .map(str::to_owned).ok_or_else(|| "manual_edit_transaction_invalid_id".to_string())
            },
            |id| clear_manual_apply_transaction(&cwd, &env, Some(id)),
            |batch, id| {
                let opts = CommitOptions {
                    cwd: &cwd,
                    env: &env,
                    page_url: None,
                    provider: provider.as_deref(),
                    timeout_ms: Some(timeout_ms),
                    apply_batch_to_source: None,
                    chat_available: None,
                    repair_only: false,
                    transaction_id: id,
                    batch: Some(batch),
                };
                commit_manual_edits_value_tracked(opts, page_url.clone())
            },
        )
    })();
    match result {
        Ok(result) => {
            println(io, &json_compact(&result));
            0
        }
        Err(message) => {
            eprintln(
                io,
                &json_compact(&json!({ "error": "commit_failed", "message": message })),
            );
            1
        }
    }
}

#[cfg(test)]
mod coordination_tests {
    use super::*;
    use std::cell::RefCell;

    fn batch() -> Value { json!({"entries":[{"id":"a","ops":[{"ref":"p"}]}]}) }
    fn success() -> Value { json!({"applied":[{}],"failed":[],"files":[],"cleared":1,"count":1,"pageUrl":"/"}) }

    #[test]
    fn success_freezes_batch_and_clears_own_transaction_after_apply() {
        let b = batch();
        let events = RefCell::new(Vec::new());
        let result = coordinate_batch(&b,
            |seen| { assert_eq!(seen, &b); events.borrow_mut().push("preflight"); Ok(()) },
            |seen| { assert_eq!(seen, &b); events.borrow_mut().push("create"); Ok("tx".into()) },
            |id| { assert_eq!(id, "tx"); events.borrow_mut().push("clear"); true },
            |seen, id| { assert_eq!(seen, &b); assert_eq!(id, Some("tx")); events.borrow_mut().push("apply"); (Ok(success()), true) },
        ).unwrap();
        assert_eq!(result["failed"], json!([]));
        assert_eq!(*events.borrow(), vec!["preflight", "create", "preflight", "apply", "clear"]);
    }

    #[test]
    fn stale_after_creation_clears_only_before_any_dispatch() {
        let calls = RefCell::new(0);
        let events = RefCell::new(Vec::new());
        let result = coordinate_batch(&batch(),
            |_| { *calls.borrow_mut() += 1; if *calls.borrow() == 2 { Err("stale".into()) } else { Ok(()) } },
            |_| Ok("tx".into()),
            |id| { assert_eq!(id, "tx"); events.borrow_mut().push("clear"); true },
            |_, _| { events.borrow_mut().push("apply"); (Ok(success()), true) },
        );
        assert_eq!(result.unwrap_err(), "stale");
        assert_eq!(*events.borrow(), vec!["clear"]);
    }

    #[test]
    fn ambiguous_or_failed_apply_preserves_transaction() {
        for result in [Err("timeout".into()), Ok(json!({"failed":[{}]})),
                       Ok(json!({"failed":[],"recoveryRequired":true})),
                       Ok(json!({"failed":[],"needsManualDecision":true})),
                       Ok(json!({"failed":[],"needsManualDecision":"unknown"})),
                       Ok(json!({}))] {
            let cleared = RefCell::new(false);
            let _ = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()),
                |_| { *cleared.borrow_mut() = true; true }, |_, _| (result, true));
            assert!(!*cleared.borrow());
        }
    }

    #[test]
    fn no_ops_preserves_empty_batch_behavior_without_transaction() {
        let empty = json!({"entries":[]});
        let result = coordinate_batch(&empty,
            |_| panic!("unexpected preflight"), |_| panic!("unexpected transaction"),
            |_| panic!("unexpected clear"), |b, id| { assert_eq!(b, &empty); assert_eq!(id, None); (Ok(json!({"reason":"no_pending_edits"})), false) }).unwrap();
        assert_eq!(result["reason"], "no_pending_edits");
    }

    #[test]
    fn failed_transaction_cleanup_is_reported() {
        let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()), |_| false,
            |_, _| (Ok(success()), true));
        assert_eq!(result.unwrap_err(), "manual_edit_transaction_clear_failed_after_apply: tx");
    }
    #[test]
    fn tracked_predispatch_error_cleans_but_dispatched_error_preserves() {
        for dispatched in [false, true] {
            let cleared = RefCell::new(false);
            let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()),
                |_| { *cleared.borrow_mut() = true; true },
                |_, _| (Err("stale".into()), dispatched));
            assert_eq!(*cleared.borrow(), !dispatched);
            let error = result.unwrap_err();
            if dispatched { assert!(error.contains("manual_edit_recovery_required: transaction tx")); }
            else { assert_eq!(error, "stale"); }
        }
    }

    #[test]
    fn ambiguous_result_names_retained_transaction_without_weakening_status() {
        let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()), |_| panic!("must preserve"),
            |_, _| (Ok(json!({"failed":[{}],"reason":"existing","needsManualDecision":true,"transactionId":"previous"})), true)).unwrap();
        assert_eq!(result["transactionId"], "tx");
        assert_eq!(result["reportedTransactionId"], "previous");
        assert_eq!(result["recoveryRequired"], true);
        assert_eq!(result["needsManualDecision"], true);
        assert_eq!(result["reason"], "existing");
    }

    #[test]
    fn unknown_or_inconsistent_result_cannot_clear_transaction() {
        let cases = [
            json!({"status":"error"}), json!({"status":"unexpected"}),
            json!({"error":{"message":"failed"}}), json!({"reason":"unknown"}),
            json!({"count":2}), json!({"cleared":0}), json!({"conflicts":[{}]}),
            json!({"rollbackFailures":[{}]}), json!({"applied":[]}),
        ];
        for extra in cases {
            let mut value = success();
            value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()),
                |_| panic!("must preserve"), |_, _| (Ok(value), true)).unwrap();
            assert_eq!(result["recoveryRequired"], true);
            assert_eq!(result["transactionId"], "tx");
        }
    }

    #[test]
    fn failed_predispatch_cleanup_identifies_retained_transaction() {
        let calls = RefCell::new(0);
        let result = coordinate_batch(&batch(),
            |_| { *calls.borrow_mut() += 1; if *calls.borrow() == 2 { Err("stale".into()) } else { Ok(()) } },
            |_| Ok("tx".into()), |_| false, |_, _| panic!("must not dispatch"));
        assert_eq!(result.unwrap_err(), "stale; manual_edit_transaction_clear_failed: tx");
    }

    fn predispatch_failure() -> Value {
        json!({"applied":[],"failed":[{"reason":"provider unavailable"}],"files":[],"cleared":0,"count":1,
               "pageUrl":"/","rolledBackFiles":[],"rollbackFailures":[{"reason":"manual_recovery_required"}],
               "needsManualDecision":true,"recoveryRequired":true})
    }

    #[test]
    fn known_pre_provider_failure_releases_only_its_transaction() {
        let cleared = RefCell::new(Vec::new());
        let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()),
            |id| { cleared.borrow_mut().push(id.to_string()); true },
            |_, _| (Ok(predispatch_failure()), false)).unwrap();
        assert_eq!(*cleared.borrow(), vec!["tx"]);
        assert_eq!(result["transactionId"], "tx");
        assert_eq!(result["transactionReleased"], true);
        assert_eq!(result["sourceChangeNotDispatched"], true);
        assert_eq!(result["sourceRollbackNotNeeded"], true);
        assert_eq!(result["rollbackFailures"], json!([]));
        assert_eq!(result["priorRollbackDiagnostics"], json!([{"reason":"manual_recovery_required"}]));
        assert_eq!(result["recoveryRequired"], false);
        assert_eq!(result["needsManualDecision"], true);
        assert!(!result["failed"].as_array().unwrap().is_empty());
    }

    #[test]
    fn dispatched_or_unrecognized_failure_never_uses_pre_dispatch_cleanup() {
        for (dispatched, extra) in [(true, json!({})), (false, json!({"rolledBackFiles":["page.html"]})),
                                    (false, json!({"transactionId":"other"})),
                                    (false, json!({"priorRollbackDiagnostics":[{"unrelated":true}]})),
                                    (false, json!({"rollbackFailures":[{"reason":"unknown"}]}))] {
            let mut failure = predispatch_failure();
            failure.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let result = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()),
                |_| panic!("must preserve transaction"), |_, _| (Ok(failure), dispatched)).unwrap();
            assert_eq!(result["transactionId"], "tx");
            assert_eq!(result["recoveryRequired"], true);
            assert!(result.get("transactionReleased").is_none());
        }
    }

    #[test]
    fn pre_dispatch_failure_cleanup_failure_keeps_record_and_reports_id() {
        let error = coordinate_batch(&batch(), |_| Ok(()), |_| Ok("tx".into()), |_| false,
            |_, _| (Ok(predispatch_failure()), false)).unwrap_err();
        assert_eq!(error, "manual_edit_transaction_clear_failed_before_dispatch: tx");
    }

    #[test]
    fn released_marker_refuses_to_overwrite_existing_diagnostics() {
        let mut failure = predispatch_failure();
        failure["priorRollbackDiagnostics"] = json!([{"existing":true}]);
        let before = failure.clone();
        assert!(!mark_predispatch_transaction_released(&mut failure, "tx", 1));
        assert_eq!(failure, before);
    }

}
