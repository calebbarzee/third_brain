//! content review (design.md §15.2) and conflict resolution (design.md §14): the two
//! provider-backed hooks that check a change's content rather than its structure.
//!
//! - `packet` gathers the review context packet: diffs, inferred operations, graph neighbors,
//!   derived metrics, all bounded to fit a prompt.
//! - `content` is the `review` hook: prompt, schema, validation, and the merge decision.
//! - `conflict` is the `resolve` hook: a conflicted path's three diffs and both sides'
//!   messages in, a merged file out, with escalation rules.
//! - `udiff` is a small in-process unified-diff generator `conflict` uses (a trial-merge tree
//!   is not a commit `git diff` can address).

mod conflict;
mod content;
mod packet;
pub mod udiff;

pub use conflict::{ConflictInput, ResolveOutput, gather_conflict, resolve, should_escalate};
pub use content::{FindingKind, ReviewDecision, ReviewFinding, ReviewOutput, Verdict, decide, packet_only, review};
pub use packet::{DerivedMetrics, DocChange, ReviewInput, ReviewPacket, SessionFacts, gather};
