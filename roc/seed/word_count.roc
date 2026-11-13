# Seed tool: counts whitespace-separated words in `text`.
# Tools are called as NAME(input_json, results) and return a Str or a request.
WordCountInput : { text : Str }

word_count : Str, List(Str) -> Str
word_count = |input, _results| {
    parsed : Try(WordCountInput, _)
    parsed = Json.parse(input)
    match parsed {
        Ok(args) => {
            spaced = args.text.replace_each("\n", " ").replace_each("\t", " ").replace_each("\r", " ")
            Str.split_on(spaced, " ").keep_if(|w| !Str.is_empty(w)).len().to_str()
        }
        Err(_) => "error: expected {\"text\": string}"
    }
}
