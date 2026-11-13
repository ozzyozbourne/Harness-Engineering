# Seed hook: builds the supplemental system prompt from notes and lessons.
compose_system : List(Str), List(Str) -> Str
compose_system = |notes, lessons| {
    note_part = if notes.is_empty() "" else Str.concat("## Notes\n", Str.join_with(notes.map(Str.trim), "\n\n"))
    lesson_part = if lessons.is_empty() "" else Str.concat("## Lessons\n", Str.join_with(lessons.map(|l| Str.concat("- ", Str.trim(l))), "\n"))
    Str.join_with([note_part, lesson_part].keep_if(|p| !Str.is_empty(p)), "\n\n")
}
