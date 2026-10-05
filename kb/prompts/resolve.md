# resolve

a rebase stopped on one conflicted document. you get the file at `base` (before either side
touched it), `ours` (the session being merged), and `theirs` (what landed on `main` since
base), plus both sides' commit messages. your job is to write the one file both sides should
end up with.

## how to merge

- keep both sides' intent when they are compatible. a section one side added and the other
  did not touch stays; a wording change to one paragraph and a new section elsewhere both
  survive in the merged file.
- when both sides changed the same fact (a number, a status, a name), prefer `theirs`: it is
  already on `main`, and `ours` is the one being rebased onto it. say so in `summary`.
- never drop a section either side added. if keeping both would duplicate content, merge the
  two into one section rather than deleting either one's material.
- frontmatter fields follow the same rule: a field only one side changed keeps that side's
  value; a field both sides changed to different values takes `theirs`'s value unless doing so
  would contradict the body you are writing, in which case explain the exception in `summary`.
- the document's `id` never changes. keep `ours`'s `id` exactly.

## when the two edits cannot both be true

if `ours` and `theirs` assert facts that contradict each other (not just different wording of
the same fact, but claims that cannot both hold), do not pick a winner. set
`"contradiction": true`, write the merged file as best you can from the non-conflicting parts,
and describe the contradiction in `summary` so a human resolves it.

## confidence

`confidence` is how sure you are that the merged file is what both sides would have wanted, not
how syntactically clean the merge was. a merge that keeps both sides' sentences intact but you
are unsure whether the combination reads coherently should still get a modest confidence.

## output

output only this json object, nothing before or after it:

```json
{
  "content": "the full merged file: frontmatter and body, ready to write to disk",
  "confidence": 0.0,
  "contradiction": false,
  "summary": "one or two sentences on what you did and why"
}
```
