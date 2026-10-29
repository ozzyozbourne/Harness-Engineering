// Seed hook `select_lessons`: the 8 highest-scored lessons sharing a word of
// 4+ letters with the task, else the top 3 by score.
use std::collections::HashSet;

fn words(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.to_lowercase())
        .collect()
}

pub fn select_lessons(task: &str, lessons: &[serde_json::Value]) -> Vec<i64> {
    let task_words = words(task);
    let mut all: Vec<(i64, i64, bool)> = lessons
        .iter()
        .filter_map(|l| {
            let id = l.get("id")?.as_i64()?;
            let score = l.get("score").and_then(|s| s.as_i64()).unwrap_or(0);
            let text = l.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Some((id, score, !words(text).is_disjoint(&task_words)))
        })
        .collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let hits: Vec<i64> = all.iter().filter(|l| l.2).take(8).map(|l| l.0).collect();
    if !hits.is_empty() {
        return hits;
    }
    all.iter().take(3).map(|l| l.0).collect()
}
