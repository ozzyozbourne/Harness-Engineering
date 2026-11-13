# Seed hook: the 8 highest-scored lessons sharing a word of 4+ letters with
# the task, else the top 3 by score.
select_lessons_words : Str -> List(Str)
select_lessons_words = |text| {
    var $words = []
    var $cur = []
    for b in Str.with_ascii_lowercased(text).to_utf8() {
        alnum = (b >= 'a' and b <= 'z') or (b >= '0' and b <= '9') or b >= 128
        if alnum {
            $cur = $cur.append(b)
        } else {
            if $cur.len() >= 4 { $words = $words.append(Str.from_utf8_lossy($cur)) } else { {} }
            $cur = []
        }
    }
    if $cur.len() >= 4 $words.append(Str.from_utf8_lossy($cur)) else $words
}

select_lessons : Str, List({ id : I64, text : Str, score : I64 }) -> List(I64)
select_lessons = |task, lessons| {
    task_words = select_lessons_words(task)
    ranked = lessons.sort_with(|a, b| if a.score > b.score Before else if a.score < b.score After else if a.id < b.id Before else if a.id > b.id After else Same)
    hits = ranked.keep_if(|l| select_lessons_words(l.text).any(|w| task_words.contains(w)))
    if hits.is_empty() ranked.take_first(3).map(|l| l.id) else hits.take_first(8).map(|l| l.id)
}
