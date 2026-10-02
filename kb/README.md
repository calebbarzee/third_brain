# kb

git-backed knowledge base for humans and agents. the design lives in [../design.md](../design.md); this file covers building and using the binary.

## build

```bash
cargo build --release
```

the binary is `target/release/kb`. put it on `PATH` or set `KB_BIN` for the pre-commit hook.

## start a knowledge base

```bash
kb init ~/notes-kb
```

creates `docs/`, `schemas/`, `prompts/`, `eval/`, `redactions/`, the three config files, a `.gitignore` with `.kb/`, a git repository on `main`, and a pre-commit hook that fills frontmatter and blocks secrets.

drop markdown files into `docs/` and commit. the hook renames each to its ulid, fills `id`, `title`, `status`, `layer`, `agent_policy`, `owner`, and migrates obsidian-style `---` yaml headers to toml. with a provider configured for the `classify` role in `kb.toml`, `layer` and `tags` are suggested by a model.

## everyday commands

| command | what it does |
|---|---|
| `kb check validate [--fix] [--staged] [paths]` | validate documents; fill missing fields |
| `kb check infer <base>..<head>` | classify a diff into operations |
| `kb check gate <base>..<head> [--agent]` | auto or review class for a change |
| `kb check graph` | hierarchy and succession graphs must be acyclic |
| `kb check secrets [--staged] [range]` | secrets gate |
| `kb index build` | build the sqlite index for the current `main` commit |
| `kb index search <query>` | full-text search over canon and draft documents |
| `kb index get <id>`, `show`, `refs`, `ls`, `blame`, `graph` | read the index |
| `kb session open --objective "..."` | open a worktree on `session/<ulid>`; prints the path last |
| `kb session settle <id>` | squash the session into one commit with a justification |
| `kb sessions`, `kb eod` | list open sessions; end-of-day settle of every open session |
| `kb merge <session> [--check] [--approve]` | rebase, checks, structure gate, content review, fast-forward onto `main` |
| `kb review list`, `approve <session>`, `reject <session> --reason` | the human review queue (git notes on session branches) |
| `kb review run <base>..<head>`, `packet`, `resolve` | the model-backed review and conflict hooks |
| `kb revert <commit|run|session>` | forward-commit revert through a new session |
| `kb doctor`, `kb tick` | history and worktree health; reconciliation |
| `kb archive capture [--source NAME]` | pull configured sources (`[sources.<name>] kind = "files" | "git"`) into the content store |
| `kb archive ls`, `show <id>`, `export <id> [--original]`, `put <file>`, `quarantine` | the capture catalog; captures never appear in default search |
| `kb curate captures --all-new` or `--ids <id>...` | turn new captures into operations inside a session; with no `curate` provider the packet is printed for the calling agent (deferred prompt) |
| `kb curate propose "<text>" --task "..." [--dry-run]` | curate a free-text proposal |
| `kb curate apply --session <id> --ops-json <file>` | apply hand-written operations; how a deferred packet completes |
| `kb answer ask "<question>"` | hybrid retrieval plus a cited answer from the `answer` provider; deferred passages when none is configured |
| `kb answer eval [--compare fts,hybrid]`, `kb answer eval-seed` | retrieval quality against `eval/queries.toml` |
| `kb import <dir> [--move] [--layer L]` | bring existing notes in through one session: frontmatter, ulids, link rewriting, source refs |
| `kb serve [--no-auth] [--read-only]`, `kb mcp`, `kb token issue <principal>` | rest and mcp over http on 127.0.0.1:7777; mcp over stdio; bearer tokens. `--read-only` refuses `propose` and the webhooks |
| `kb schedule install [--serve]`, `status`, `uninstall` | systemd user timers for tick, capture, eod, nightly, backup |
| `kb maintain nightly [--dry-run]`, `findings`, `escalations`, `compact` | nightly curation, escalations, compaction after a checkpoint |
| `kb backup run [--kind mirror|bundle|store-copy|all]`, `manifest`, `verify`, `restore-drill <dir>` | backups per `backup.toml` |
| `kb status` | one screen of repository, sessions, index, captures, backups, scheduler |
| `kb run list`, `show`, `verify`, `diff`, `search` | run logs |
| `kb transcript list [--since 24h] [--role R] [--kind llm\|mcp]`, `kb transcript show <id>` | the full request and reply of every model call and mcp tool call, from `.kb/state.sqlite` |
| `kb validate`, `kb secrets`, `kb search`, `kb get`, `kb show` | aliases for the `check` and `index` forms above |
| `kb provider test <role>`, `kb provider classify <file> [--write]` | provider connectivity and manual classify |
| `kb gate ask --labels <a,b,...> [--context "..."] <text>` | run the quick gate (role `gate`) by hand |

ids accept a full ulid or a unique prefix. `--json` gives machine-readable output. `--repo <path>` points at a repository other than the current one.

## providers

```toml
# kb.toml
[providers.classify]
kind = "ollama"            # anthropic | openai | ollama | claude-code | command
model = "llama3.2:3b"
base_url = "http://localhost:11434"
# api_key_env = "ANTHROPIC_API_KEY"   # name of the variable; never the key

# runs the owner's locally installed `claude` cli in print mode, so requests ride whatever
# subscription that login already has and need no api key. `model` is passed to `--model`
# verbatim: an alias ("sonnet", "opus", "haiku") or a full model name both work; leave it empty
# to take the cli's own default model. `base_url` is not used.
[providers.gate]
kind = "claude-code"
model = "haiku"

# runs an arbitrary local command, feeding it the flattened prompt on stdin and reading its
# reply from stdout: a hand-rolled classifier plugs into any role without a dedicated adapter.
# `base_url` holds the whole command line, split on whitespace (no quoting -- a path with a
# space in it does not work). `model` and `api_key_env` are ignored.
[providers.gate]
kind = "command"
base_url = "python3 /path/to/classifier.py"
```

every completion through any role is stored in full in the `transcripts` table of `.kb/state.sqlite`, with its actor, run, session, and trigger (`KB_TRIGGER`: the systemd units set `timer:<job>`, the pre-commit hook `hook:pre-commit`, `kb mcp` uses `mcp:stdio`, `kb serve` uses `http`, otherwise `cli`). the `claude-code` adapter passes `--no-session-persistence`, so no copy lands in `~/.claude/projects`.

`KB_PROVIDER_TIMEOUT_SECS` (default 600) bounds a request, including the `claude-code` and `command` adapters' child process (killed past the deadline). `KB_ANTHROPIC_THINKING_BUDGET` enables extended thinking on the anthropic adapter. `KB_CLAUDE_CODE_BIN` overrides the `claude` binary the `claude-code` adapter runs (default: `claude`, resolved on `PATH`).

### the quick gate

role `gate` is a cheap classifier, meant for a small local model or a short script rather than a
full model call: `kb gate ask --labels <a,b,...> <text>` runs it by hand, and `kb merge` runs it
automatically ahead of the full content review (design.md §15.2) for a human session the
structure gate already classified `auto`. when `gate` answers `label = "trivial"` at or above
`thresholds.review_confidence`, the full review is skipped and the merge proceeds on the quick
gate's word alone; otherwise, or with no `gate` provider configured, or for an agent session,
the full review runs exactly as it always has. every time the gate is actually asked, its answer
is recorded as a `check` event in the run log, regardless of which way it came out.

## use it from an agent

register the stdio server in an mcp client (claude code, cursor, or any mcp client):

```json
{ "mcpServers": { "kb": { "command": "/path/to/kb", "args": ["--repo", "/path/to/your-kb", "mcp"] } } }
```

tools: `search`, `get`, `refs`, `propose`, `run`, `answer`. `propose` opens a session and applies validated operations; the agent then runs `kb merge <session>` or leaves it for `kb eod`.

## approval rule

`manifest.toml [thresholds] agent_auto_merge = false` (the default) makes every agent session review class: it waits in `kb review list` until a person runs `kb review approve`. any change outside `docs/*.md` (config, prompts, schemas, groups) is review class for humans too. turn `agent_auto_merge` on only after the review evaluation gate in design.md §31 passes.

## simulate days

`KB_NOW` (rfc 3339) fixes kb's clock: timestamps, ages, new ulids, and the dates on commits kb makes. `scripts/simulate.py` uses it to run a knowledge base through many simulated days with live models:

```bash
python3 scripts/simulate.py --out ../.tmp/sim_$(date +%F) --generations 12
```

it writes `simulation.log` (every command, landed diff, review verdict, and answer), `metrics.tsv`, `report.md`, and `gen_NN/` snapshots.

## develop

```bash
cargo test                       # unit and integration suites
KB_LIVE_PROVIDER_TESTS=1 cargo test -- --ignored   # live provider tests when a key or ollama is present
```

modules map to design sections: `parse` §6 to §7, `check` §8 to §11 and §15.1 and §20, `index` §17 to §18, `provider` §19, `session` §12, `runlog` §16, `policy` §10, `review` §14 to §15.2, `merge` §13 and §22.
