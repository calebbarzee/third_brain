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
| `kb run list`, `show`, `verify`, `diff`, `search` | run logs |
| `kb validate`, `kb secrets`, `kb search`, `kb get`, `kb show` | aliases for the `check` and `index` forms above |
| `kb provider test <role>`, `kb provider classify <file> [--write]` | provider connectivity and manual classify |

ids accept a full ulid or a unique prefix. `--json` gives machine-readable output. `--repo <path>` points at a repository other than the current one.

## providers

```toml
# kb.toml
[providers.classify]
kind = "ollama"            # anthropic | openai | ollama
model = "llama3.2:3b"
base_url = "http://localhost:11434"
# api_key_env = "ANTHROPIC_API_KEY"   # name of the variable; never the key
```

`KB_PROVIDER_TIMEOUT_SECS` (default 600) bounds a request. `KB_ANTHROPIC_THINKING_BUDGET` enables extended thinking on the anthropic adapter.

## develop

```bash
cargo test                       # unit and integration suites
KB_LIVE_PROVIDER_TESTS=1 cargo test -- --ignored   # live provider tests when a key or ollama is present
```

modules map to design sections: `parse` §6 to §7, `check` §8 to §11 and §15.1 and §20, `index` §17 to §18, `provider` §19, `session` §12, `runlog` §16, `policy` §10, `review` §14 to §15.2, `merge` §13 and §22.
