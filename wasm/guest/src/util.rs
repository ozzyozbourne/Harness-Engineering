//! Small text and JSON helpers.

use serde_json::Value;
use std::collections::HashSet;

/// String field `k` of a JSON object ("" if missing or not a string).
pub fn str_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}
/// Unsigned field `k` (0 if missing).
pub fn u32_field(v: &Value, k: &str) -> u32 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0) as u32
}
/// Signed field `k` (0 if missing).
pub fn i64_field(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... (truncated, {} bytes total)", &s[..cut], s.len())
}

/// At most `max` characters of `s`, with "..." if anything was cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}...", s.chars().take(max).collect::<String>()) }
}

/// The first non-blank line of `s`, trimmed and clipped to `max` characters.
pub fn first_line(s: &str, max: usize) -> String {
    clip(s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), max)
}

pub fn compact(v: &Value, max: usize) -> String {
    first_line(&v.to_string(), max)
}

pub fn words(s: &str, min: usize) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= min).map(str::to_lowercase).collect()
}

/// Lexically normalised absolute path: `p` joined to `base` if relative,
/// with `.` and `..` resolved. (WASI has no canonicalize; the host checks
/// real paths again on every write.)
pub fn normalize(base: &str, p: &str) -> String {
    let joined = if p.starts_with('/') { p.to_string() } else { format!("{}/{p}", base.trim_end_matches('/')) };
    let mut parts: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}
