//! Pure migration decision prototype. This module is test-only: the runtime
//! cannot yet prove an old writer has stopped, so it must not expose a route.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterEvidence {
    Unknown,
    ActiveOrTransactionPresent,
    // A hypothetical proof used only in pure tests. The running application
    // does not yet have a trustworthy producer for this evidence.
    VerifiedStoppedAndNoTransaction,
}

struct Review<'a> {
    // The exact operation and source bytes explicitly reviewed by the user.
    entry: &'a Value,
    source_file: &'a str,
    source_bytes: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
enum Reject {
    MissingReview,
    WriterUncertain,
    InvalidEntry,
    EntryChanged,
    InvalidOperation,
    AlreadyVersioned,
    ExistingConflict,
    UnsupportedRemoval,
    SourceUncertain,
    SourceChanged,
    InvalidRevision,
}

// Returns an in-memory candidate only. A future caller would need to hold the
// transaction and buffer locks, establish writer evidence, and commit using
// compare-and-swap. Nothing here opens files or updates the installed engine.
fn plan_upgrade(
    review: Option<Review<'_>>,
    frozen: &Value,
    current: &Value,
    reference: &str,
    writer: WriterEvidence,
    current_source: Option<(&str, &[u8])>,
    new_revision: &str,
) -> Result<Value, Reject> {
    let review = review.ok_or(Reject::MissingReview)?;
    if writer != WriterEvidence::VerifiedStoppedAndNoTransaction {
        return Err(Reject::WriterUncertain);
    }
    if frozen.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).is_none()
        || frozen.get("pageUrl").and_then(Value::as_str).filter(|s| !s.is_empty()).is_none()
        || reference.is_empty()
    {
        return Err(Reject::InvalidEntry);
    }
    if review.entry != frozen || current != frozen {
        return Err(Reject::EntryChanged);
    }
    let ops = frozen.get("ops").and_then(Value::as_array).ok_or(Reject::InvalidEntry)?;
    let indexes: Vec<usize> = ops.iter().enumerate().filter_map(|(index, op)| {
        (op.get("ref").and_then(Value::as_str) == Some(reference)).then_some(index)
    }).collect();
    if indexes.len() != 1 { return Err(Reject::InvalidOperation); }
    let op = &ops[indexes[0]];
    if !op.is_object() { return Err(Reject::InvalidOperation); }
    if op.get("_impeccableRevision").is_some() { return Err(Reject::AlreadyVersioned); }
    if op.get("_impeccableSourceConflict").is_some() { return Err(Reject::ExistingConflict); }
    if op.get("removeElement") == Some(&Value::Bool(true)) { return Err(Reject::UnsupportedRemoval); }
    let hint = op.get("sourceHint").and_then(|hint| hint.get("file")).and_then(Value::as_str)
        .filter(|s| !s.is_empty()).ok_or(Reject::SourceUncertain)?;
    let (actual_file, actual_bytes) = current_source.ok_or(Reject::SourceUncertain)?;
    if hint != review.source_file || actual_file != review.source_file || actual_bytes != review.source_bytes {
        return Err(Reject::SourceChanged);
    }
    if new_revision.len() != 32 || !new_revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Reject::InvalidRevision);
    }
    let mut upgraded = current.clone();
    upgraded["ops"][indexes[0]].as_object_mut().ok_or(Reject::InvalidOperation)?
        .insert("_impeccableRevision".into(), Value::String(new_revision.to_owned()));
    Ok(upgraded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REVISION: &str = "0123456789abcdef0123456789abcdef";
    fn entry() -> Value {
        json!({"id":"one", "pageUrl":"/", "ops":[
            {"ref":"p", "originalText":"old", "newText":"new", "sourceHint":{"file":"index.html"}},
            {"ref":"h1", "originalText":"heading", "newText":"title", "sourceHint":{"file":"index.html"}}
        ]})
    }
    fn review(entry: &Value) -> Review<'_> {
        Review { entry, source_file: "index.html", source_bytes: b"<p>old</p>" }
    }
    fn run(entry: &Value, current: &Value, writer: WriterEvidence, source: Option<(&str, &[u8])>) -> Result<Value, Reject> {
        plan_upgrade(Some(review(entry)), entry, current, "p", writer, source, REVISION)
    }
    #[test]
    fn confirmed_stable_legacy_operation_only_changes_the_selected_revision() {
        let before = entry();
        let upgraded = run(&before, &before, WriterEvidence::VerifiedStoppedAndNoTransaction, Some(("index.html", b"<p>old</p>"))).unwrap();
        let mut expected = before.clone();
        expected["ops"][0]["_impeccableRevision"] = json!(REVISION);
        assert_eq!(upgraded, expected);
        assert!(before["ops"][0].get("_impeccableRevision").is_none());
    }
    #[test]
    fn requires_explicit_review_and_verified_writer_stop() {
        let before = entry();
        assert_eq!(plan_upgrade(None, &before, &before, "p", WriterEvidence::VerifiedStoppedAndNoTransaction, Some(("index.html", b"<p>old</p>")), REVISION), Err(Reject::MissingReview));
        for writer in [WriterEvidence::Unknown, WriterEvidence::ActiveOrTransactionPresent] {
            assert_eq!(run(&before, &before, writer, Some(("index.html", b"<p>old</p>"))), Err(Reject::WriterUncertain));
        }
    }
    #[test]
    fn refuses_changed_entry_source_or_missing_source() {
        let before = entry();
        let mut newer = before.clone();
        newer["ops"][0]["newText"] = json!("newer");
        assert_eq!(run(&before, &newer, WriterEvidence::VerifiedStoppedAndNoTransaction, Some(("index.html", b"<p>old</p>"))), Err(Reject::EntryChanged));
        assert_eq!(run(&before, &before, WriterEvidence::VerifiedStoppedAndNoTransaction, Some(("index.html", b"changed"))), Err(Reject::SourceChanged));
        assert_eq!(run(&before, &before, WriterEvidence::VerifiedStoppedAndNoTransaction, None), Err(Reject::SourceUncertain));
    }
    #[test]
    fn refuses_existing_conflicts_versions_removals_and_ambiguous_references() {
        let mut candidates = vec![];
        let mut conflicting = entry(); conflicting["ops"][0]["_impeccableSourceConflict"] = json!({}); candidates.push((conflicting, Reject::ExistingConflict));
        let mut versioned = entry(); versioned["ops"][0]["_impeccableRevision"] = json!("old"); candidates.push((versioned, Reject::AlreadyVersioned));
        let mut removal = entry(); removal["ops"][0]["removeElement"] = json!(true); candidates.push((removal, Reject::UnsupportedRemoval));
        let mut duplicate = entry(); duplicate["ops"][1]["ref"] = json!("p"); candidates.push((duplicate, Reject::InvalidOperation));
        for (candidate, expected) in candidates {
            assert_eq!(run(&candidate, &candidate, WriterEvidence::VerifiedStoppedAndNoTransaction, Some(("index.html", b"<p>old</p>"))), Err(expected));
        }
    }
}
