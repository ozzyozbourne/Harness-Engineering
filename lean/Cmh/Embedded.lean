/-! Build-time copies of the base prompt and the seed userspace. -/
namespace Cmh.Embedded

/-- Fallback copy of `../base-prompt.md`, embedded at build time. -/
def basePrompt : String := include_str "../../base-prompt.md"

def seedManifest : String := include_str "../userspace/seeds.json"

def seedSource : String → Option String
  | "word_count.lean" => some (include_str "../userspace/word_count.lean")
  | "compose_system.lean" => some (include_str "../userspace/compose_system.lean")
  | "select_lessons.lean" => some (include_str "../userspace/select_lessons.lean")
  | _ => none

end Cmh.Embedded
