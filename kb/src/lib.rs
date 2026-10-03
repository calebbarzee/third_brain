//! kb: a git-backed knowledge base for humans and agents.
//!
//! module map (see ../../design.md for the design):
//! - `model`    shared types: frontmatter, layers, statuses, refs, sections. the contract every module builds on.
//! - `parse`    frontmatter split and toml parse, body parser (sections, slug paths, inline links).
//! - `check`    validate --fix, infer (diff to operations), structure gate, graph acyclicity, secrets gate.
//! - `index`    sqlite index build (documents, chunks, fts5, refs, derived from git, criticality) and queries.
//! - `provider` llm provider trait with anthropic, openai, ollama adapters; classify hook with retry loop.
//! - `config`   kb.toml, manifest.toml, groups.toml loading and defaults.
//! - `git`      thin wrappers over the git binary.
//! - `session`  session lifecycle: open, settle, eod, cleanup, worktree hygiene.
//! - `runlog`   the run log: one hash-chained, append-only sqlite file per agent run.
//! - `policy`   the agent policy function: access, ceiling, budget, provenance, merge class.
//! - `review`   content review (design.md §15.2) and conflict resolution (design.md §14).
//! - `merge`    `kb merge`: fetch, rebase, the three checks, land, review class, tick, history.
//! - `store`    the content store: blobs, the capture catalog, normalizers, source adapters.
//! - `curate`   the `curate` hook: captures or a free-text proposal into operations in a session.
//! - `answer`   the `answer` hook: search, answer from the passages, cite, and record usage;
//!   also `kb eval`'s mrr@10/recall@5 over `eval/queries.toml` (design.md §18, §31).
//! - `server`   design.md §23 (local and server): tokens, rest, mcp (stdio and http), `kb serve`.
//! - `maintain` `curate-nightly` (design.md §15.3): detectors, auto-fix, escalations, compaction.
//! - `backup`   design.md §21: mirror push, bundles, content-store copy, the backups manifest.
//! - `import`   `kb import`: existing markdown notes into documents (design.md §6, §9, §12).
//! - `schedule` `kb schedule`: systemd user units (or crontab lines) for tick, capture, eod,
//!   nightly, and backups.
//! - `status`   `kb status`: one screen summarizing repository health.
//! - `cli`      clap command definitions and handlers.

pub mod answer;
pub mod backup;
pub mod check;
pub mod cli;
pub mod clock;
pub mod config;
pub mod curate;
pub mod git;
pub mod import;
pub mod index;
pub mod maintain;
pub mod merge;
pub mod model;
pub mod parse;
pub mod policy;
pub mod provider;
pub mod review;
pub mod runlog;
pub mod schedule;
pub mod server;
pub mod session;
pub mod status;
pub mod store;
pub mod transcript;
