//! JS: live-discard-manual-edits.mjs -> `impeccable
//! live-discard-manual-edits`. Drops pending manual edits from the buffer
//! without touching source files.

use crate::event_validation::truthy;
use crate::live_commit_manual_edits::arg_val;
use crate::manual_edits::buffer::update_buffer;
use crate::util::{json_compact, println};
use impeccable_common::Io;
use serde_json::{json, Map, Value};

pub fn run(args: &[String], io: &mut Io) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println(
            io,
            "Usage: impeccable live-discard-manual-edits [--page-url=<url>]",
        );
        return 0;
    }

    let cwd = io.cwd.to_string_lossy().into_owned();
    let env = io.env.clone();
    let page_url_filter = arg_val(args, "--page-url");

    let result = crate::manual_edits::apply::with_no_active_transaction(&cwd, &env, || update_buffer(&cwd, &env, |buffer| {
        let mut entries = Vec::new();
        let mut kept = Vec::new();
        let mut discarded = 0usize;
        for entry in std::mem::take(&mut buffer.entries) {
            let matches = !truthy(Some(&page_url_filter)) || entry.get("pageUrl") == Some(&page_url_filter);
            if matches {
                discarded += entry.get("ops").and_then(Value::as_array).map_or(0, Vec::len);
                entries.push(entry);
            } else { kept.push(entry); }
        }
        let remaining = kept.iter().map(|entry| entry.get("ops").and_then(Value::as_array).map_or(0, Vec::len)).sum::<usize>();
        buffer.entries = kept;
        Ok((entries, discarded, remaining))
    }));
    let (entries, discarded, remaining) = match result {
        Ok(result) => result,
        Err(error) => { io.err(&format!("{error}\n")); return 1; }
    };

    let mut m = Map::new();
    m.insert("discarded".into(), json!(discarded));
    m.insert("entries".into(), Value::Array(entries));
    m.insert("totalCount".into(), json!(remaining));
    println(io, &json_compact(&Value::Object(m)));
    0
}
