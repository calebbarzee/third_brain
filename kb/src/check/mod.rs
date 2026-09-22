//! `kb validate`, `infer`, the structure gate, graph acyclicity, and the secrets gate.
//! contract: design.md §6, §7, §8, §11, §15.1, §20.

pub mod banner;
pub mod gate;
pub mod graph;
pub mod infer;
pub mod secrets;
pub mod validate;

pub use banner::{banner as render_banner, has_banner};
pub use gate::{GateDecision, classify_change};
pub use graph::check_graph;
pub use infer::{InferredOp, infer, infer_range};
pub use secrets::{scan_range, scan_staged, scan_text};
pub use validate::{RepoValidateOutcome, ValidateOutcome, migrate_legacy_frontmatter, missing_classifiable, validate_file, validate_repo};
