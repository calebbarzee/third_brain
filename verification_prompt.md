# verification prompt

paste this into a fresh session (claude code or another agent with shell access) to verify the knowledge system rigorously. it is written for an agent, in the second person.

---

you are verifying `kb`, a git-backed knowledge base for humans and agents. project: `/home/calebbarzee/1_projects/dev/third_brain`. read `design.md` fully before anything else; it is the specification. the crate is `kb/`; `kb/README.md` lists the commands. build with `cd kb && cargo build --release`; use `kb/target/release/kb`.

rules: work only in throwaway directories you create under `/home/calebbarzee/1_projects/dev/third_brain/.tmp/verify_<name>/`. never touch `~/2_resources/notes` (the owner's live deployment), `~/3_archives`, or systemd units. do not install anything. models: ollama on localhost may have `llama3.2:3b` and `nomic-embed-text`; the `claude` cli is available for the `claude-code` provider kind and costs subscription usage, so use `haiku` and keep live calls to what a scenario needs. every scenario below must be run for real, not reasoned about. record for each: the exact commands, observed output, pass or fail against the design section cited, and the run ids involved.

## part 1: static verification

1. `cargo test`, `cargo clippy --all-targets`, `cargo doc --no-deps` in `kb/`: all clean. record counts.
2. read every module's public api against its design section (module map in `kb/src/lib.rs`). list every design mechanism that has no implementation and every implementation that has no design section. cite section numbers.
3. `RUSTFLAGS="-D warnings" cargo build --all-targets`.

## part 2: invariants (design §4)

for each invariant write a scenario that tries to break it and report what happened.

1. ulid filenames: create a file named `notes.md` with an id of another document; a copy of a document with the same id; an id with a future timestamp.
2. humans never blocked: as a human, delete a canon document, rewrite a principle, change `owner`, remove a required heading. each must land only via review class, never a denial, and `kb check validate` must fail only for parse errors, missing forward pointers, cycles, or secrets.
3. agents only through offered operations: with `kb curate propose` and a fake or live provider, attempt an op on a document outside the candidates, an op not in `allowed_ops`, a `create` without a source ref, an append past the layer budget. each must be refused with the documented reason and recorded in the run log.
4. every change through `kb merge`: try `git push` to a bare remote configured with `receive.denyNonFastForwards`; try `git commit` directly on `main` and confirm the hook runs and what it allows; confirm `kb doctor` detects a hand reset of `main`.
5. monotonic history: `kb revert` a merged session; assert history length grows and the reverted content is recoverable; try to make `kb` rewrite `main` (redaction, compaction) and confirm it refuses.
6. responsible person: an agent session's settled commit names the principal's owner in `Co-authored-by`; contributors in the index resolve to a person.
7. derived data never in the file: grep `docs/` for `updated`, `word_count`, `criticality`; edit a file by hand and confirm `kb show` reports `uncommitted` with the mtime and the artifact still holds the committed value.
8. acyclic graphs: introduce a parent cycle and a succession cycle by hand; validate and merge must fail; `kb maintain findings` must report one that slipped in through `git commit --no-verify`.
9. captures out of default search: put a capture with a unique word; default search misses it, `--include-captures` finds it, a `source` ref reaches it.
10. secrets never reach main: stage a fake github token, an aws key, a private key block, a slack webhook, and a custom rule from `kb.toml`; each must block the hook, `settle`, and `merge`; the quarantine path on `kb archive put` must trigger; confirm no secret text appears in any run log or finding.

## part 3: integrated workflows (design part iii)

run each end to end in one throwaway kb seeded by `kb import` from copies of `.tmp/fixtures/notes/` (12 notes), with providers configured as: classify `claude-code haiku`, embed `ollama nomic-embed-text`, gate `ollama llama3.2:3b`, review `claude-code haiku`. record wall time per step.

1. human writes a document (§24): body only, commit on main, hook fills frontmatter, index rebuilt by `kb tick`, searchable within one tick.
2. two sessions touch one document (§26): disjoint lines land both; same lines stop at `needs-human` with the worktree left mid-rebase; a human resolution then lands; with `resolve` configured to `claude-code haiku`, an agent session's conflict resolves or escalates per §14.
3. capture to canon (§25): a `files` source, `kb archive capture`, `kb curate captures --all-new` with curate `claude-code haiku`, dry run then real, `kb merge` of the result. assert the settled message carries the justification, the run log has policy decisions per op, the capture status becomes `curated`, and `discard` was chosen for at least one capture.
4. ask a question (§27): `kb answer ask` with answer `claude-code haiku`; citations resolve to passages that were given; a bogus citation is dropped and noted; usage rows appear in `state.sqlite`; the next `kb index build` shows non-zero `reads_90d`.
5. review queue: a `principle` layer change as an agent goes to review class; `kb review list` shows it; `kb review reject` sends it back; `kb review approve` lands it with the approver recorded in the note.
6. misleading justification: a session whose message says "fix typo" but deletes a section; content review must request changes or escalate; `kb merge --approve` overrides with the override recorded.
7. quick gate: a whitespace-only change as a human with the gate configured; the run log shows the gate answer and whether the full review was skipped; the same change as an agent must not skip.
8. end of day: an open session with uncommitted work; `kb eod` settles it with `eod = true`; the review packet carries session facts; a half-written section yields a `completeness` finding.
9. nightly (§15.3): seed a stale draft older than `draft_ttl_days` (backdated ulid), a superseded document without a banner, an over-budget document, a dangling `superseded_by`; `kb maintain nightly --dry-run` lists the right kinds; a real run with `min_corpus = 1` auto-fixes the banner, escalates over-budget and dangling, sends the delete to review class, and tags a checkpoint only on a clean pass.
10. backups and compaction (§21): `backup.toml` with a local bare mirror and bundle dir; `kb backup run --kind all`; `kb backup verify`; `kb backup restore-drill` reports no differences; a `delete` is refused until the file's last commit is bundled; `kb maintain compact` after a checkpoint prunes merged session branches and leaves `main` history intact.
11. server and mcp (§23): `kb serve --no-auth` on an ephemeral port: `/health`, `/search`, `/doc/<id>`, `/answer`, `/mcp` `tools/call search`; `kb token issue`, a request with and without a token, a revoked token; `kb mcp` over stdio with `kb/scripts/mcp_smoke.sh`; register it in a client if one is available and call `search` from there.
12. import at scale: generate 500 synthetic notes with wiki links and yaml headers; `kb import --move`; assert time, link rewrite counts, aliases, and that `git log --follow` shows history on a moved file.
13. reconciliation (§22): stop the tick, merge two sessions, delete the newest index artifact, run `kb tick` once: index rebuilt, merged sessions cleaned, orphan worktree removed.
14. eval (§31): `kb answer eval-seed`, then `kb answer eval --compare fts,hybrid`; report mrr and recall and whether the enable rule passes.

## part 4: concurrency and failure injection

1. run two `kb merge` for different sessions at the same instant against a bare remote; both land, in some order, with no lost commits.
2. kill `kb index build` mid-way (SIGKILL): no partial artifact is ever opened; the next build succeeds; stale `.tmp-*` files are cleaned.
3. kill an agent `kb curate` run mid-way: the run log shows the last event, the session worktree remains, `kb tick` after 24 hours (simulate by editing `touched_at`) abandons it with a `wip` commit.
4. corrupt one byte in a sealed run log: `kb run verify` names the first bad sequence.
5. remove ollama from the network (set `base_url` to a closed port): builds skip vectors with a note, gate falls back to full review, nothing panics.
6. make `main` move between a session's rebase and land (a second writer on the bare remote): the retry loop lands after re-running checks; after five failures the session goes to review with a note.

## part 5: report

write `.tmp/verify_report_<date>.md`: a table of every scenario with pass or fail and the design section; a ranked defect list with exact repro commands and observed versus expected; timing table for the model-backed steps; the list of design mechanisms not yet implemented; and your own judgment of the three riskiest gaps for daily use. do not fix anything unless a scenario cannot run at all without the fix; in that case make the smallest change, say what it was, and continue.
