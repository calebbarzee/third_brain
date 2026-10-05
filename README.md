# third brain

a git-backed knowledge base for humans and agents: plain-text documents with toml frontmatter, every change through a reviewed session, every agent action in a run log, everything derived rebuildable.

| file | purpose |
|---|---|
| [design.md](design.md) | the authoritative design and build plan |
| [quickstart.md](quickstart.md) | build the binary and run a knowledge base in ten minutes |
| [operating_guide.md](operating_guide.md) | daily use of the deployed instance on `~/2_resources/notes` |
| [verification_prompt.md](verification_prompt.md) | the prompt for a rigorous verification and workflow-test session |
| [future_ideas.md](future_ideas.md) | ideas that are not part of the design |
| [kb/](kb/) | the rust crate; [kb/README.md](kb/README.md) lists every command |

## state

design build steps 0 to 6 are implemented. the crate has 398 tests and is clippy clean. the system runs locally: git worktrees for sessions, sqlite for the index and run logs, ollama for embeddings and the quick gate, the local `claude` cli for classify, review, curate, and answer.

## build

```bash
cd kb && cargo build --release && ln -sfn "$PWD/target/release/kb" ~/.local/bin/kb
```

## layout

```
design.md            what the system is and how it is built
quickstart.md        first run
operating_guide.md   the deployed instance
kb/                  crate: src/{parse,check,index,provider,policy,session,runlog,review,merge,store,curate,maintain,backup,answer,server,import,schedule,status,cli}
.tmp/                planning reports, fixtures, mock knowledge bases (ignored)
```
