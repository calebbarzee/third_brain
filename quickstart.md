# quickstart

ten minutes from a folder of markdown to a searchable, reviewed knowledge base with an agent interface. commands assume `kb` is on your path (see [README.md](README.md)).

## 1. requirements

- git 2.38 or newer, rust toolchain (for the build)
- optional, for models: [ollama](https://ollama.com) with `ollama pull nomic-embed-text` (embeddings) and `ollama pull llama3.2:3b` (quick gate); the `claude` cli for classify, review, curate, and answer on your subscription, or an anthropic or openai api key
- optional: systemd user session for the timers

without any model the system still works: frontmatter is filled deterministically, search is full-text, merges skip content review, and `kb answer` returns the passages for you or your agent to read.

## 2. create a knowledge base

```bash
mkdir ~/kb && kb init ~/kb && cd ~/kb
```

`init` writes `kb.toml`, `manifest.toml`, `groups.toml`, `backup.toml` is yours to add, a `docs/` folder, a git repository on `main`, and a pre-commit hook.

configure providers in `kb.toml` (any subset):

```toml
[providers.classify]
kind = "claude-code"      # anthropic | openai | ollama | claude-code | command
model = "haiku"

[providers.review]
kind = "claude-code"
model = "sonnet"

[providers.answer]
kind = "claude-code"
model = "sonnet"

[providers.embed]
kind = "ollama"
model = "nomic-embed-text"
base_url = "http://localhost:11434"

[providers.gate]
kind = "ollama"
model = "llama3.2:3b"
base_url = "http://localhost:11434"
```

`kb provider test classify` checks a role.

## 3. bring in existing notes

```bash
kb import ~/old-notes            # copies; add --move inside this repo to keep git history
kb index build
kb status
```

import fills frontmatter, migrates obsidian yaml headers, renames files to their ulid, rewrites `[[wiki]]` and relative links to `kb:` uris, keeps the old name in `aliases`, and lands everything through one session.

## 4. write, find, ask

```bash
kb session open --objective "note on x"      # prints a worktree path; write docs/anything.md there
kb session settle <id> -m "why"
kb merge <id>                                # checks, structure gate, content review, fast-forward

kb search "static ip"
kb show <id-prefix>
kb answer ask "how do we set a static ip?"
```

or edit `docs/` on `main` and commit: the hook fills and validates the frontmatter.

## 5. connect an agent

```bash
claude mcp add kb -- "$(command -v kb)" --repo ~/kb mcp
```

tools: `search`, `get`, `refs`, `answer`, `propose`, `run`. or `kb serve --no-auth` for rest and mcp over http on 127.0.0.1:7777.

## 6. let it run

```bash
kb schedule install         # tick 60 s, capture hourly, eod 02:00, nightly 03:00, backups
kb backup run --kind all    # needs backup.toml: mirror, bundle_dir, store_copy
kb status
```

## 7. undo

`kb schedule uninstall`; `rm -rf .kb` drops derived state; the repository history holds every document version.
