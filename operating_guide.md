+++
id = "01M35F0QWCX1MW2MDGN3VDFCP7"
layer = "procedure"
title = "operating guide: daily driving the knowledge base"
status = "draft"
agent_policy = "editable"
owner = "caleb"
tags = ["operations", "knowledge-base"]
parent = "kb:01M32EH22Q4MFV9P72M6Y7CRKY"

[access]
read = ["*"]
write = ["owner"]
+++

# operating guide: daily driving the knowledge base

the knowledge base is deployed at `~/2_resources/notes`. the `kb` binary is `~/.local/bin/kb` (a symlink to `kb/target/release/kb` in this project). [design.md](design.md) is the reference for every mechanism named here.

## where things live

| what | where |
|---|---|
| documents | `~/2_resources/notes/docs/<ulid>.md`; the original filename is in `source_id` and `aliases` |
| configuration | `kb.toml` (providers, store, sources), `manifest.toml` (budgets, thresholds), `groups.toml` (empty: single-user mode), `backup.toml` |
| state, index, sessions, run logs, content store | `~/2_resources/notes/.kb/` (ignored by git) |
| backups | `~/3_archives/kb_notes_mirror.git`, `~/3_archives/kb_notes_bundles/`, `~/3_archives/kb_notes_store/` |
| scheduler | systemd user timers `kb-tick@notes`, `kb-capture`, `kb-eod` (02:00), `kb-nightly` (03:00), `kb-backup` (hourly), `kb-backup-full` (04:00) |
| providers | classify haiku, review sonnet, resolve sonnet, curate sonnet, answer sonnet through the local `claude` cli; embeddings `nomic-embed-text` and the quick gate `llama3.2:3b` through ollama |

## write a note

1. `cd ~/2_resources/notes && kb session open --objective "what you are about to do"` prints a worktree path. work there, or edit `docs/` directly on `main` and let `git commit` run the pre-commit hook.
2. write markdown in `docs/`. any filename works; the hook fills the frontmatter, migrates yaml headers, classifies layer and tags, and renames the file to its ulid.
3. `kb session settle <id> -m "why"` squashes the session into one commit. `kb merge <id>` rebases, runs the checks, asks the content reviewer, and fast-forwards `main`. a review-class change waits for `kb review approve <id>`; a change the reviewer rejects comes back to you, and `kb merge <id> --approve` lands it over the objection with your name on it.
4. or do nothing: `kb eod` settles every open session at 02:00 and requests merges.

## find and ask

```bash
kb search "static ip jetson"            # hybrid full-text and vector search over canon and draft
kb index search "..." --explain         # ranks and fused scores per hit
kb show 01M35                           # frontmatter, derived block, body; prefixes work
kb answer ask "how do i set a static ip between the laptop and the jetson?"
kb index refs <id> --incoming           # who links here
kb index graph --top 10                 # criticality
```

captures never appear in search unless you ask: `kb index search "..." --include-captures`.

## use it from an agent

```bash
claude mcp add kb -- ~/.local/bin/kb --repo ~/2_resources/notes mcp
```

the agent gets `search`, `get`, `refs`, `answer`, `propose`, and `run`. `propose` opens a session and applies validated operations; you land it with `kb merge` or leave it for the nightly pass. every agent action is in a run log: `kb run list --since 24h`, `kb run show <id>`.

## gather context

add a source to `kb.toml` and the hourly capture pulls it into the content store:

```toml
[sources.inbox]
kind = "files"
path = "/home/calebbarzee/2_resources/notes/inbox"
glob = "**/*"
```

`kb archive ls`, `kb archive show <id>`, `kb archive export <id>` read captures back. `kb curate captures --all-new --task "..."` turns them into proposed operations in a session (sonnet), with a dry run available.

## keep it healthy

| when | what |
|---|---|
| any time | `kb status` |
| after a bad merge | `kb revert <session or commit>` (a forward commit through a new session) |
| conflict left in a worktree | resolve with git in the printed path, then `kb merge <id>` again |
| review queue | `kb review list`, `kb review approve <id>`, `kb review reject <id> --reason "..."` |
| nightly findings | `kb maintain findings`, `kb maintain escalations`; the nightly pass auto-fixes only low-risk findings and tags a checkpoint when clean |
| history and worktrees | `kb doctor`; `kb maintain compact` after a checkpoint |
| backups | `kb backup manifest`, `kb backup verify`, `kb backup restore-drill /tmp/drill` |
| retrieval quality | `kb answer eval-seed` once, then `kb answer eval --compare fts,hybrid` |

## undo the deployment

everything is git or ignored state: `git log` in the notes repository shows the import as one commit (`git revert` restores the old filenames); `kb schedule uninstall` removes the timers; `rm -rf .kb` drops derived state; the backups in `~/3_archives` are plain git mirrors and bundles.
