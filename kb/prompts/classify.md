# classify

task: read one document's title and body and suggest its `layer` and up to 6 `tags`. this
fills empty frontmatter fields only; a human always keeps the ability to override it
(design.md §6).

## layers, most to least resistant to change

- `principle`: a rule the system never breaks. changed only with two-human review from
  `architecture`.
- `design`: how a part of the system works. changed with two-human review.
- `decision`: one choice made and its consequences. changed with one-human review.
- `procedure`: steps for a repeatable task. edited freely within the criticality band.
- `reference`: material that supports the other layers: definitions, logs, generated output.
  edited freely within the band. the default when nothing else fits.

## how to tell the layers apart

read the body's shape before its topic.

- mostly numbered steps, shell commands, or "do x then y" for a repeatable task: `procedure`.
- tables, pinouts, glossaries, cheat sheets, logs, lists of facts with little argument: `reference`.
- "we chose x over y because", a dated choice with consequences or trade-offs: `decision`.
- explains how a system or component works: parts, data flow, interfaces, diagrams: `design`.
- a short standing rule that other documents must obey: `principle`. rare; prefer another layer when unsure.
- a note that is a todo list, meeting notes, or a debugging log is `reference`.

## tag rules

- 1 to 6 tags.
- lowercase kebab-case, matching `^[a-z0-9][a-z0-9-]{0,31}$`.
- prefer words already used in the existing document titles given to you, so the tag
  vocabulary stays small across the corpus.
- a tag names a topic or component, not a layer or a status; those are separate fields.

## output

output only this json object, nothing before or after it:

```json
{"layer": "principle | design | decision | procedure | reference", "tags": ["...", "..."], "confidence": 0.0, "reason": "one sentence"}
```

`confidence` is a number from 0 to 1. `reason` is one sentence stating why, in plain
language.
