//! sqlite index: schema, build, query. see design.md §17-18 and lib.rs module map.
pub mod build;
mod criticality;
mod gitlog;
mod graph;
pub mod prefix;
pub mod query;
pub mod schema;
pub mod vectors;

pub use build::{BuildReport, RefRow, build, build_with_providers, newest_artifact, open_newest, refresh_captures};
pub use prefix::{PrefixReport, build_prefixes, deterministic_prefix};
pub use query::{
    BlameRow, CaptureFields, DocRecord, Forward, LsFilter, LsRow, SearchExplain, SearchHit, SearchMode, SearchOpts, ShowResult, blame, get, ls, refs,
    resolve_id, search, show,
};
pub use vectors::{VectorReport, build_vectors, register_extension as register_vector_extension};
