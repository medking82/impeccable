//! Strict read-only access to the shared pending-manual-edits buffer.

use crate::util::Env;
use serde_json::Value;

pub fn read_buffer(cwd: &str, env: &Env) -> Result<Vec<Value>, String> {
    Ok(crate::manual_edits::buffer::read_buffer_strict(cwd, env)?.entries)
}
