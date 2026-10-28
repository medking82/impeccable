//! JS: live/manual-edits-buffer.mjs. The pending-manual-edits buffer on disk
//! (`.impeccable/live/pending-manual-edits.json`).

use crate::json_error::json_parse_error;
use crate::paths::live_dir;
use crate::util::{iso_now, jsp, Env};
use serde_json::{json, Map, Value};

pub const BUFFER_VERSION: i64 = 1;
const BUFFER_FILENAME: &str = "pending-manual-edits.json";

/// JS: getBufferPath(cwd)
pub fn buffer_path(cwd: &str, env: &Env) -> String {
    jsp::join(&[&live_dir(cwd, env), BUFFER_FILENAME])
}

/// The parsed buffer: `{ version: 1, entries: [...] }`.
#[derive(Debug, Clone)]
pub struct Buffer {
    pub entries: Vec<Value>,
}

impl Buffer {
    pub fn to_value(&self) -> Value {
        json!({ "version": BUFFER_VERSION, "entries": self.entries })
    }
}

fn validate_schema(parsed: &Value) -> Result<(), String> {
        let object = parsed.as_object().ok_or("manual_edit_buffer_unreadable: manual_edit_buffer_invalid_schema")?;
        let mut ids = std::collections::HashSet::new();
        let mut page_refs = std::collections::HashSet::new();
        if object.get("version").and_then(Value::as_i64) != Some(BUFFER_VERSION)
            || object.keys().any(|key| key != "version" && key != "entries")
            || !object.get("entries").and_then(Value::as_array).map_or(false, |entries| entries.iter().all(|entry| {
                let Some(id) = entry.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return false };
                let Some(page) = entry.get("pageUrl").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return false };
                let Some(ops) = entry.get("ops").and_then(Value::as_array).filter(|ops| !ops.is_empty()) else { return false };
                ids.insert(id.to_string()) && ops.iter().all(|op| {
                    op.as_object().is_some() && op.get("ref").and_then(Value::as_str).filter(|s| !s.is_empty())
                        .map_or(false, |reference| page_refs.insert((page.to_string(), reference.to_string())))
                })
            })) {
            return Err("manual_edit_buffer_unreadable: manual_edit_buffer_invalid_schema".into());
        }
    Ok(())
}

fn read_internal(cwd: &str, env: &Env, strict: bool) -> Result<Buffer, String> {
    let file = buffer_path(cwd, env);
    let raw = match std::fs::read(&file) {
        Ok(b) => match String::from_utf8(b) {
            Ok(raw) => raw,
            Err(_) if strict => return Err("manual_edit_buffer_unreadable: invalid UTF-8".into()),
            Err(_) => return Ok(Buffer { entries: vec![] }),
        },
        Err(e) => {
            if strict && e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "manual_edit_buffer_unreadable: {}",
                    impeccable_context::util::node_read_error(&file, &e)
                ));
            }
            return Ok(Buffer { entries: vec![] });
        }
    };
    let parsed: Option<Value> = serde_json::from_str(&raw).ok();
    let parsed = match parsed {
        Some(v) => v,
        None => {
            if strict {
                let msg = json_parse_error(&raw).unwrap_or_else(|| "Unexpected token".to_string());
                return Err(format!("manual_edit_buffer_unreadable: {}", msg));
            }
            return Ok(Buffer { entries: vec![] });
        }
    };
    if strict { validate_schema(&parsed)?; }
    match parsed.get("entries") {
        Some(Value::Array(a)) if parsed.is_object() => Ok(Buffer { entries: a.clone() }),
        _ => {
            if strict {
                // JS: throw new Error('manual_edit_buffer_invalid_schema') is
                // caught by the same catch and re-thrown as unreadable.
                return Err(
                    "manual_edit_buffer_unreadable: manual_edit_buffer_invalid_schema".to_string(),
                );
            }
            Ok(Buffer { entries: vec![] })
        }
    }
}

/// JS: readBuffer(cwd)
pub fn read_buffer(cwd: &str, env: &Env) -> Buffer {
    read_internal(cwd, env, false).unwrap_or(Buffer { entries: vec![] })
}

/// JS: readBufferStrict(cwd)
pub fn read_buffer_strict(cwd: &str, env: &Env) -> Result<Buffer, String> {
    read_internal(cwd, env, true)
}

// Every cooperating writer locks the same permanent sibling file. The kernel
// releases this lock on process exit; never unlink the lock file.
fn lock_buffer(cwd: &str, env: &Env) -> Result<std::fs::File, String> {
    let file = buffer_path(cwd, env);
    let parent = std::path::Path::new(&file).parent().ok_or("buffer has no parent")?;
    let locks = parent.join("locks");
    std::fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(locks.join("pending-manual-edits.lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| format!("manual_edit_buffer_lock_failed: {e}"))?;
    Ok(lock)
}

// All cooperating writers hold the shared lock for this call. The repository's
// atomic writer stages a sibling and replaces the destination without first
// deleting it; a failed replacement leaves the prior buffer available.
fn write_buffer_unlocked(cwd: &str, env: &Env, buffer: &Buffer) -> Result<(), String> {
    let file = buffer_path(cwd, env);
    impeccable_common::fsx::write_atomic(&file, crate::util::json_pretty(&buffer.to_value()).as_bytes())
        .map_err(|e| format!("manual_edit_buffer_write_failed: {e}"))
}

/// A complete, cross-process-coordinated read/modify/write transaction.
/// The closure must not call buffer APIs or take unrelated application locks.
pub fn update_buffer<T>(cwd: &str, env: &Env, update: impl FnOnce(&mut Buffer) -> Result<T, String>) -> Result<T, String> {
    let _lock = lock_buffer(cwd, env)?;
    let mut buffer = read_internal(cwd, env, true)?;
    let before = buffer.to_value();
    let result = update(&mut buffer)?;
    validate_schema(&buffer.to_value())?;
    if buffer.to_value() != before { write_buffer_unlocked(cwd, env, &buffer)?; }
    Ok(result)
}

/// Replace a buffer under the same lock; callers performing read/modify/write
/// must use update_buffer rather than passing an earlier snapshot here.
pub fn write_buffer(cwd: &str, env: &Env, buffer: &Buffer) -> Result<(), String> {
    let _lock = lock_buffer(cwd, env)?;
    read_internal(cwd, env, true)?;
    write_buffer_unlocked(cwd, env, buffer)
}

fn new_revision() -> Result<String, String> {
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|e| format!("manual_edit_revision_failed: {e}"))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

fn ops_of(entry: &Value) -> Vec<Value> {
    entry
        .get("ops")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default()
}

// Legacy operations have no verified source baseline. Restaging one must not
// silently turn it into a versioned operation that can pass commit preflight.
fn has_revision(op: &Value) -> bool {
    op.get("_impeccableRevision").and_then(Value::as_str).filter(|r| !r.is_empty()).is_some()
}

fn ensure_stage_merges_versioned(buffer: &Buffer, new_entry: &Value) -> Result<(), String> {
    let page = new_entry.get("pageUrl");
    let new_ops = ops_of(new_entry);
    for entry in &buffer.entries {
        if entry.get("pageUrl") != page { continue; }
        for old_op in ops_of(entry) {
            if new_ops.iter().any(|new_op| new_op.get("ref") == old_op.get("ref")) && !has_revision(&old_op) {
                return Err("manual_edit_legacy_revision_required".into());
            }
        }
    }
    Ok(())
}

// Undo is a new event for a versioned operation, but must not manufacture a
// verified revision for an unversioned operation from an old buffer.
fn restore_superseded_op(prior: &Value, removal: &Value, fresh_revision: impl FnOnce() -> Result<String, String>) -> Result<Value, String> {
    let mut restored = prior.clone();
    let object = restored.as_object_mut().ok_or("manual_edit_buffer_invalid_op")?;
    // An Apply may attach a conflict to the removal after it supersedes the
    // prior op. Undo must carry it forward, without erasing a prior conflict.
    if let Some(outer_conflict) = removal.get("_impeccableSourceConflict") {
        let conflict = match prior.get("_impeccableSourceConflict") {
            Some(prior_conflict) => json!({
                "reason": "multiple_conflicts",
                "prior": prior_conflict,
                "removal": outer_conflict,
            }),
            None => outer_conflict.clone(),
        };
        object.insert("_impeccableSourceConflict".into(), conflict);
    }
    if has_revision(prior) {
        object.insert("_impeccableRevision".into(), Value::String(fresh_revision()?));
    }
    Ok(restored)
}

/// JS: stageEntry(cwd, newEntry)
pub fn stage_entry(cwd: &str, env: &Env, new_entry: &Value) -> Result<Buffer, String> {
    let mut refs = std::collections::HashSet::new();
    if new_entry.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).is_none()
        || new_entry.get("pageUrl").and_then(Value::as_str).filter(|s| !s.is_empty()).is_none()
        || !new_entry.get("ops").and_then(Value::as_array).filter(|ops| !ops.is_empty()).map_or(false, |ops| ops.iter().all(|op| {
            op.as_object().is_some() && op.get("ref").and_then(Value::as_str).filter(|s| !s.is_empty()).map_or(false, |r| refs.insert(r.to_string()))
        })) {
        return Err("manual_edit_buffer_invalid_entry".into());
    }
    update_buffer(cwd, env, |buf| {
    ensure_stage_merges_versioned(buf, new_entry)?;
    let page_url = new_entry.get("pageUrl").cloned().unwrap_or(Value::Null);
    let new_id = new_entry.get("id").cloned().unwrap_or(Value::Null);
    let element = new_entry.get("element").cloned();
    for mut new_op in ops_of(new_entry) {
        // A fresh opaque revision on every stage defeats A -> B -> A ABA.
        // Never trust a caller-supplied revision or a wall-clock timestamp.
        let revision = new_revision()?;
        new_op.as_object_mut().ok_or("manual_edit_buffer_invalid_op")?
            .insert("_impeccableRevision".into(), Value::String(revision));
        let mut merged = false;
        for existing in buf.entries.iter_mut() {
            if existing.get("pageUrl") != Some(&page_url) {
                continue;
            }
            let existing_ops = ops_of(existing);
            let idx = existing_ops
                .iter()
                .position(|op| op.get("ref") == new_op.get("ref"));
            if let Some(i) = idx {
                let mut replaced = new_op.as_object().cloned().unwrap_or_default();
                // A new browser stage cannot silently clear an unresolved source conflict.
                if let Some(conflict) = existing_ops[i].get("_impeccableSourceConflict") {
                    replaced.insert("_impeccableSourceConflict".into(), conflict.clone());
                }
                let original = existing_ops[i]
                    .get("originalText")
                    .cloned()
                    .unwrap_or(Value::Null);
                replaced.insert("originalText".into(), original);
                replaced.insert(
                    "newText".into(),
                    new_op.get("newText").cloned().unwrap_or(Value::Null),
                );
                let deleted = match new_op.get("deleted") {
                    Some(v) if crate::event_validation::truthy(Some(v)) => v.clone(),
                    _ => Value::Bool(false),
                };
                replaced.insert("deleted".into(), deleted);
                // A removal that lands on an element with a staged copy edit
                // supersedes it, but keeps it: undoing the removal puts the
                // copy edit back, since the page still shows the edited text.
                let removes = new_op.get("removeElement") == Some(&Value::Bool(true));
                let existing_removes =
                    existing_ops[i].get("removeElement") == Some(&Value::Bool(true));
                if removes && !existing_removes {
                    let mut kept = existing_ops[i].as_object().cloned().unwrap_or_default();
                    kept.remove("supersededOp");
                    replaced.insert("supersededOp".into(), Value::Object(kept));
                } else if removes && existing_removes {
                    // Retried removals are idempotent: keep the copy edit that
                    // the first removal superseded, if there was one.
                    if let Some(prior) = existing_ops[i].get("supersededOp").filter(|p| p.is_object()) {
                        replaced.insert("supersededOp".into(), prior.clone());
                    }
                }
                let mut ops = existing_ops.clone();
                ops[i] = Value::Object(replaced);
                if let Some(obj) = existing.as_object_mut() {
                    obj.insert("ops".into(), Value::Array(ops));
                    if let Some(el) = &element {
                        if crate::event_validation::truthy(Some(el)) {
                            obj.insert("element".into(), el.clone());
                        }
                    }
                    obj.insert("stagedAt".into(), Value::String(iso_now()));
                }
                merged = true;
                break;
            }
        }
        if merged {
            continue;
        }
        let pos = buf
            .entries
            .iter()
            .position(|e| e.get("pageUrl") == Some(&page_url) && e.get("id") == Some(&new_id));
        let pos = match pos {
            Some(p) => p,
            None => {
                let mut m = Map::new();
                m.insert("id".into(), new_id.clone());
                m.insert("pageUrl".into(), page_url.clone());
                m.insert("element".into(), element.clone().unwrap_or(Value::Null));
                m.insert("ops".into(), Value::Array(vec![]));
                m.insert("stagedAt".into(), Value::String(iso_now()));
                buf.entries.push(Value::Object(m));
                buf.entries.len() - 1
            }
        };
        if let Some(obj) = buf.entries[pos].as_object_mut() {
            let mut ops = obj
                .get("ops")
                .and_then(|o| o.as_array())
                .cloned()
                .unwrap_or_default();
            ops.push(new_op.clone());
            obj.insert("ops".into(), Value::Array(ops));
            obj.insert("stagedAt".into(), Value::String(iso_now()));
        }
    }
    Ok(buf.clone())
    })
}

/// JS: removeEntries(cwd, predicate). Returns removed op count.
pub fn remove_entries(
    cwd: &str,
    env: &Env,
    predicate: impl Fn(&Value) -> bool,
) -> Result<usize, String> {
    update_buffer(cwd, env, |buf| {
    let mut removed = 0;
    let mut kept = Vec::new();
    for entry in std::mem::take(&mut buf.entries) {
        if predicate(&entry) {
            removed += ops_of(&entry).len();
        } else if !ops_of(&entry).is_empty() {
            kept.push(entry);
        }
    }
    buf.entries = kept;
    Ok(removed)
    })
}

/// Drop the ops on `page_url` whose `ref` is `op_ref` (the browser's Undo for
/// one staged removal); an entry left with no ops goes with them. Returns the
/// removed ops grouped by their entry (each a copy of the entry carrying only
/// the removed ops, so the browser can restore from it) and the op count.
pub fn remove_ops_by_ref(
    cwd: &str,
    env: &Env,
    page_url: &str,
    op_ref: &str,
) -> Result<(Vec<Value>, usize), String> {
    update_buffer(cwd, env, |buf| {
    let mut removed = Vec::new();
    let mut count = 0usize;
    let mut kept = Vec::new();
    for entry in std::mem::take(&mut buf.entries) {
        if entry.get("pageUrl").and_then(|p| p.as_str()) != Some(page_url) {
            kept.push(entry);
            continue;
        }
        let (gone, mut stay): (Vec<Value>, Vec<Value>) = ops_of(&entry)
            .into_iter()
            .partition(|op| op.get("ref").and_then(|r| r.as_str()) == Some(op_ref));
        // The op a removal superseded comes back in its place.
        for op in &gone {
            if let Some(prior) = op.get("supersededOp").filter(|p| p.is_object()) {
                stay.push(restore_superseded_op(prior, op, new_revision)?);
            }
        }
        if !gone.is_empty() {
            count += gone.len();
            let mut snapshot = entry.as_object().cloned().unwrap_or_default();
            snapshot.insert("ops".into(), Value::Array(gone));
            removed.push(Value::Object(snapshot));
        }
        if !stay.is_empty() {
            let mut rest = entry.as_object().cloned().unwrap_or_default();
            rest.insert("ops".into(), Value::Array(stay));
            kept.push(Value::Object(rest));
        }
    }
    buf.entries = kept;
    Ok((removed, count))
    })
}

/// JS: countByPage(cwd) -> (totalCount, perPage)
pub fn count_by_page(cwd: &str, env: &Env) -> (usize, Map<String, Value>) {
    count_buffer(&read_buffer(cwd, env))
}

/// Count the same validated snapshot that is returned to the caller.
pub fn count_buffer(buf: &Buffer) -> (usize, Map<String, Value>) {
    let mut per_page: Map<String, Value> = Map::new();
    let mut total = 0usize;
    for entry in &buf.entries {
        let n = ops_of(entry).len();
        let key = match entry.get("pageUrl") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => "null".to_string(),
            Some(other) => other.to_string(),
        };
        let cur = per_page.get(&key).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        per_page.insert(key, json!(cur + n));
        total += n;
    }
    (total, per_page)
}

/// JS: truncateBuffer(cwd). Returns removed op count.
pub fn truncate_buffer(cwd: &str, env: &Env) -> Result<usize, String> {
    update_buffer(cwd, env, |buf| {
        let removed: usize = buf.entries.iter().map(|e| ops_of(e).len()).sum();
        buf.entries.clear();
        Ok(removed)
    })
}

/// Count a strictly validated snapshot, failing on damaged or unreadable data.
pub fn count_by_page_value_strict(cwd: &str, env: &Env) -> Result<Map<String, Value>, String> {
    let buffer = read_buffer_strict(cwd, env)?;
    let (total, per_page) = count_buffer(&buffer);
    let mut result = Map::new();
    result.insert("totalCount".into(), json!(total));
    result.insert("perPage".into(), Value::Object(per_page));
    Ok(result)
}

/// `{ totalCount, perPage }` as a JSON object (spread helper).
pub fn count_by_page_value(cwd: &str, env: &Env) -> Map<String, Value> {
    let (total, per_page) = count_by_page(cwd, env);
    let mut m = Map::new();
    m.insert("totalCount".into(), json!(total));
    m.insert("perPage".into(), Value::Object(per_page));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_restage_preflight_is_atomic_and_checks_page_and_ref() {
        let buffer = Buffer { entries: vec![json!({ "id": "old", "pageUrl": "/", "ops": [
            { "ref": "p", "originalText": "A", "newText": "B" },
            { "ref": "h", "originalText": "T", "newText": "U", "_impeccableRevision": "r1" }
        ] })] };
        let before = buffer.to_value();
        let requested = json!({ "id": "new", "pageUrl": "/", "ops": [{ "ref": "h" }, { "ref": "p" }] });
        assert_eq!(ensure_stage_merges_versioned(&buffer, &requested).unwrap_err(), "manual_edit_legacy_revision_required");
        assert_eq!(buffer.to_value(), before);
        assert!(ensure_stage_merges_versioned(&buffer, &json!({ "pageUrl": "/", "ops": [{ "ref": "h" }] })).is_ok());
        assert!(ensure_stage_merges_versioned(&buffer, &json!({ "pageUrl": "/other", "ops": [{ "ref": "p" }] })).is_ok());
        assert!(ensure_stage_merges_versioned(&buffer, &json!({ "pageUrl": "/", "ops": [{ "ref": "other" }] })).is_ok());
        for invalid in [json!(""), json!(null), json!(7)] {
            let mut altered = buffer.clone();
            altered.entries[0]["ops"][0]["_impeccableRevision"] = invalid;
            assert!(ensure_stage_merges_versioned(&altered, &requested).is_err());
        }
    }

    #[test]
    fn legacy_undo_does_not_create_a_revision_and_versioned_undo_refreshes_it() {
        let old = json!({ "ref": "p", "originalText": "A", "newText": "B" });
        let restored = restore_superseded_op(&old, &json!({}), || panic!("legacy revision must not be manufactured")).unwrap();
        assert_eq!(restored, old);
        assert!(!has_revision(&restored));
        let mut versioned = old.clone();
        versioned["_impeccableRevision"] = json!("old");
        let restored = restore_superseded_op(&versioned, &json!({}), || Ok("fresh".to_string())).unwrap();
        assert_eq!(restored["_impeccableRevision"], "fresh");
        assert_eq!(versioned["_impeccableRevision"], "old");
        assert_eq!(restore_superseded_op(&versioned, &json!({}), || Err("random_failed".into())).unwrap_err(), "random_failed");
    }

    #[test]
    fn undo_retains_outer_inner_and_combined_conflicts() {
        let outer = json!({"reason":"source_changed_during_apply", "id":"outer"});
        let inner = json!({"reason":"already_conflicted", "id":"inner"});
        let prior = json!({"ref":"p", "_impeccableRevision":"old"});
        let restored = restore_superseded_op(&prior, &json!({"_impeccableSourceConflict":outer}), || Ok("fresh".into())).unwrap();
        assert_eq!(restored["_impeccableSourceConflict"], outer);
        let with_inner = json!({"ref":"p", "_impeccableRevision":"old", "_impeccableSourceConflict":inner});
        let restored = restore_superseded_op(&with_inner, &json!({}), || Ok("fresh".into())).unwrap();
        assert_eq!(restored["_impeccableSourceConflict"], inner);
        let restored = restore_superseded_op(&with_inner, &json!({"_impeccableSourceConflict":outer}), || Ok("fresh".into())).unwrap();
        assert_eq!(restored["_impeccableSourceConflict"]["prior"], inner);
        assert_eq!(restored["_impeccableSourceConflict"]["removal"], outer);
        assert_eq!(restored["_impeccableRevision"], "fresh");
        // Preflight rejects the presence of the key, including a null value.
        let null_outer = restore_superseded_op(&prior, &json!({"_impeccableSourceConflict":null}), || Ok("fresh".into())).unwrap();
        assert_eq!(null_outer.get("_impeccableSourceConflict"), Some(&Value::Null));
        let null_inner_prior = json!({"ref":"p", "_impeccableRevision":"old", "_impeccableSourceConflict":null});
        let null_inner = restore_superseded_op(&null_inner_prior, &json!({}), || Ok("fresh".into())).unwrap();
        assert_eq!(null_inner.get("_impeccableSourceConflict"), Some(&Value::Null));
        let null_both = restore_superseded_op(&null_inner_prior, &json!({"_impeccableSourceConflict":null}), || Ok("fresh".into())).unwrap();
        let combined = null_both.get("_impeccableSourceConflict").unwrap();
        assert_eq!(combined.get("prior"), Some(&Value::Null));
        assert_eq!(combined.get("removal"), Some(&Value::Null));
        let legacy = json!({"ref":"p", "_impeccableSourceConflict":inner});
        let restored = restore_superseded_op(&legacy, &json!({"_impeccableSourceConflict":outer}), || panic!("legacy operation must not gain a revision")).unwrap();
        assert!(!has_revision(&restored));
        assert_eq!(restored["_impeccableSourceConflict"]["prior"], inner);
        assert_eq!(restored["_impeccableSourceConflict"]["removal"], outer);
        assert_eq!(legacy["_impeccableSourceConflict"], inner);
    }

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "impeccable-buffer-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn remove_ops_by_ref_drops_one_op_and_then_the_emptied_entry() {
        let cwd = scratch("ref");
        let env = Env::new();
        stage_entry(
            &cwd,
            &env,
            &json!({ "id": "e1", "pageUrl": "/", "element": {}, "ops": [
                { "ref": "main>p", "tag": "p", "originalText": "A", "newText": "B" },
                { "ref": "main>h1", "tag": "h1", "originalText": "T", "deleted": true, "removeElement": true },
            ] }),
        )
        .unwrap();
        stage_entry(
            &cwd,
            &env,
            &json!({ "id": "e2", "pageUrl": "/other", "element": {}, "ops": [
                { "ref": "main>h1", "tag": "h1", "originalText": "X", "newText": "Y" },
            ] }),
        )
        .unwrap();

        let (removed, n) = remove_ops_by_ref(&cwd, &env, "/", "main>h1").unwrap();
        assert_eq!(n, 1);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0]["id"], "e1");
        assert_eq!(removed[0]["ops"].as_array().unwrap().len(), 1);
        assert_eq!(removed[0]["ops"][0]["removeElement"], true);

        // e1 keeps its text op; the other page's op with the same ref is untouched.
        let buf = read_buffer(&cwd, &env);
        assert_eq!(buf.entries.len(), 2);
        assert_eq!(ops_of(&buf.entries[0]).len(), 1);
        assert_eq!(buf.entries[0]["ops"][0]["ref"], "main>p");
        assert_eq!(ops_of(&buf.entries[1]).len(), 1);

        let (_, n) = remove_ops_by_ref(&cwd, &env, "/", "main>p").unwrap();
        assert_eq!(n, 1);
        let buf = read_buffer(&cwd, &env);
        assert_eq!(buf.entries.len(), 1);
        assert_eq!(buf.entries[0]["id"], "e2");

        let (removed, n) = remove_ops_by_ref(&cwd, &env, "/", "nope").unwrap();
        assert_eq!((removed.len(), n), (0, 0));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn a_removal_supersedes_a_staged_copy_edit_and_undo_restores_it() {
        let cwd = scratch("supersede");
        let env = Env::new();
        stage_entry(
            &cwd,
            &env,
            &json!({ "id": "e1", "pageUrl": "/", "element": {}, "ops": [
                { "ref": "main>p", "tag": "p", "originalText": "A", "newText": "B" },
            ] }),
        )
        .unwrap();
        let buf = stage_entry(
            &cwd,
            &env,
            &json!({ "id": "e2", "pageUrl": "/", "element": {}, "ops": [
                { "ref": "main>p", "tag": "p", "originalText": "B", "deleted": true, "removeElement": true },
            ] }),
        )
        .unwrap();
        // Merged by ref into e1: one removal op that remembers the copy edit
        // and keeps the copy edit's originalText (the source still says A).
        assert_eq!(buf.entries.len(), 1);
        let op = &buf.entries[0]["ops"][0];
        assert_eq!(op["removeElement"], true);
        assert_eq!(op["originalText"], "A");
        assert_eq!(op["supersededOp"]["newText"], "B");

        // A retried or duplicated removal must retain the superseded copy edit.
        stage_entry(
            &cwd,
            &env,
            &json!({ "id": "e3", "pageUrl": "/", "element": {}, "ops": [
                { "ref": "main>p", "tag": "p", "originalText": "B", "deleted": true, "removeElement": true },
            ] }),
        )
        .unwrap();

        let (removed, n) = remove_ops_by_ref(&cwd, &env, "/", "main>p").unwrap();
        assert_eq!(n, 1);
        assert_eq!(removed[0]["ops"][0]["removeElement"], true);
        let buf = read_buffer(&cwd, &env);
        assert_eq!(buf.entries.len(), 1);
        let op = &buf.entries[0]["ops"][0];
        assert_eq!(op.get("removeElement"), None);
        assert_eq!(op["newText"], "B");
        assert_eq!(op.get("supersededOp"), None);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}


#[cfg(test)]
mod consistency_tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("impeccable-buffer-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    fn op(id: &str, page: &str, reference: &str, text: &str) -> Value {
        json!({"id":id,"pageUrl":page,"element":{},"ops":[{"ref":reference,"originalText":"A","newText":text}]})
    }
    #[test]
    fn corrupt_or_ambiguous_buffers_never_get_overwritten() {
        let dir = scratch("corrupt"); let cwd = dir.to_string_lossy().to_string(); let env = Env::new();
        let path = PathBuf::from(buffer_path(&cwd, &env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cases: Vec<Vec<u8>> = vec![b"{\"version\":1,\"entries\":[".to_vec(),
            b"{\"version\":1,\"entries\":[],\"x\":\"\xff\"}".to_vec(),
            br#"{"version":2,"entries":[]}"#.to_vec(),
            br#"{"version":1,"entries":[],"future":true}"#.to_vec(),
            br#"{"version":1,"entries":[{"id":"a","pageUrl":"/","ops":[{"ref":"x"},{"ref":"x"}]}]}"#.to_vec(),
            br#"{"version":1,"entries":[{"id":"a","pageUrl":"/","ops":[{"ref":"x"}]},{"id":"a","pageUrl":"/two","ops":[{"ref":"y"}]}]}"#.to_vec(),
            br#"{"version":1,"entries":[{"id":"a","pageUrl":"/","ops":[{"ref":"x"}]},{"id":"b","pageUrl":"/","ops":[{"ref":"x"}]}]}"#.to_vec(),
            br#"{"version":1,"entries":[{"id":"a","ops":[{"ref":"x"}]}]}"#.to_vec()];
        for original in cases {
            std::fs::write(&path, &original).unwrap();
            assert!(stage_entry(&cwd, &env, &op("new", "/", "new", "B")).is_err());
            assert!(remove_entries(&cwd, &env, |_| true).is_err());
            assert!(remove_ops_by_ref(&cwd, &env, "/", "x").is_err());
            assert!(truncate_buffer(&cwd, &env).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn every_stage_and_undo_restoration_receives_a_fresh_revision() {
        let dir=scratch("revisions"); let cwd=dir.to_string_lossy().to_string(); let env=Env::new();
        let first=stage_entry(&cwd,&env,&op("a","/","x","B")).unwrap();
        let r1=first.entries[0]["ops"][0]["_impeccableRevision"].as_str().unwrap().to_owned();
        let second=stage_entry(&cwd,&env,&op("b","/","x","C")).unwrap();
        let r2=second.entries[0]["ops"][0]["_impeccableRevision"].as_str().unwrap().to_owned();
        let third=stage_entry(&cwd,&env,&op("c","/","x","B")).unwrap();
        let r3=third.entries[0]["ops"][0]["_impeccableRevision"].as_str().unwrap().to_owned();
        let remove=json!({"id":"d","pageUrl":"/","element":{},"ops":[{"ref":"x","originalText":"B","deleted":true,"removeElement":true}]});
        stage_entry(&cwd,&env,&remove).unwrap(); remove_ops_by_ref(&cwd,&env,"/","x").unwrap();
        let restored=read_buffer_strict(&cwd,&env).unwrap();
        let r4=restored.entries[0]["ops"][0]["_impeccableRevision"].as_str().unwrap().to_owned();
        assert_eq!(HashSet::from([r1,r2,r3,r4]).len(),4);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn buffer_lock_uses_existing_ignored_locks_directory() {
        let dir = scratch("ignored-lock");
        let cwd = dir.to_string_lossy().to_string();
        stage_entry(&cwd, &Env::new(), &op("id", "/", "ref", "B")).unwrap();
        let lock = dir.join(".impeccable/live/locks/pending-manual-edits.lock");
        assert!(lock.is_file(), "the shared lock should be in the existing ignored directory");
        assert!(crate::gitignore::LIVE_IGNORE_PATTERNS.contains(&".impeccable/live/locks/"));
        assert!(!dir.join(".impeccable/live/pending-manual-edits.lock").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn process_stage_worker() {
        let Ok(cwd) = std::env::var("IMPECCABLE_BUFFER_TEST_ROOT") else { return };
        let env=Env::new(); let pid=std::process::id();
        for i in 0..20 { let key=format!("{pid}-{i}"); stage_entry(&cwd,&env,&op(&key,"/",&key,"B")).unwrap(); }
    }
    #[test]
    fn cooperating_processes_do_not_lose_staged_entries() {
        let dir=scratch("process"); let cwd=dir.to_string_lossy().to_string();
        let exe=std::env::current_exe().unwrap(); let mut children=Vec::new();
        for _ in 0..4 {
            let mut command=Command::new(&exe);
            command.arg("--exact").arg("manual_edits::buffer::consistency_tests::process_stage_worker")
                .arg("--nocapture").env("IMPECCABLE_BUFFER_TEST_ROOT",&cwd)
                .stdout(Stdio::null()).stderr(Stdio::piped());
            #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
            children.push(command.spawn().unwrap());
        }
        for child in children { let output=child.wait_with_output().unwrap(); assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr)); }
        let buffer=read_buffer_strict(&cwd,&Env::new()).unwrap();
        assert_eq!(buffer.entries.len(),80);
        let _=std::fs::remove_dir_all(dir);
    }
}
