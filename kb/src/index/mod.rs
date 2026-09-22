//! sqlite index: schema, build, query. see design.md §17-18 and lib.rs module map.
pub mod build;
mod criticality;
mod gitlog;
mod graph;
pub mod query;
pub mod schema;

pub use build::{BuildReport, RefRow, build, newest_artifact, open_newest, refresh_captures};
pub use query::{
    BlameRow, CaptureFields, DocRecord, Forward, LsFilter, LsRow, SearchHit, SearchOpts, ShowResult, blame, get, ls, refs, resolve_id, search, show,
};
