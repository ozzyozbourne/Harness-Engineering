// Seed hook `compose_system`: builds the supplemental system prompt.
// The kernel always prepends the immutable base prompt itself.
pub fn compose_system(notes: &[String], lessons: &[String]) -> String {
    let mut out = String::new();
    if !notes.is_empty() {
        out.push_str("## Notes\n");
        for note in notes {
            out.push_str(note.trim());
            out.push_str("\n\n");
        }
    }
    if !lessons.is_empty() {
        out.push_str("## Lessons\n");
        for lesson in lessons {
            out.push_str("- ");
            out.push_str(lesson.trim());
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}
