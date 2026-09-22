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
//! - `cli`      clap command definitions and handlers.

pub mod model;
pub mod parse;
pub mod check;
pub mod index;
pub mod provider;
pub mod config;
pub mod git;
pub mod session;
pub mod runlog;
pub mod policy;
pub mod review;
pub mod merge;
pub mod store;
pub mod curate;
pub mod cli;
