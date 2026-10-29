// Seed tool `word_count`: {"text": string} -> number of whitespace-separated words.
pub fn run(input: &serde_json::Value) -> Result<String, String> {
    let text = input
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or("missing string field `text`")?;
    Ok(text.split_whitespace().count().to_string())
}
