//! the content store: gathered context that never lives in git. contract: design.md §9 (content
//! store), §19 (the `capture` hook), §20 (secrets scanned at store `put`), §17 (the index treats
//! catalog rows like documents, tagged `kind = capture`).
//!
//! - `blob`      content-addressed blob storage (`<store>/blobs/<aa>/<sha256>`), local backend.
//! - `catalog`   the capture catalog: one row per captured item, in `<state>/state.sqlite`.
//! - `normalize` bytes-to-text normalization per media type, versioned.
//! - `sources`   adapters (`files`, `git`) that pull external content into captures.
//! - `export`    `kb archive export`: normalized text or raw bytes to a file.
//! - `usage`     `read`/`cite` events per document per day, aggregated into `graph_metrics` at
//!   index build time (design.md §17.3, §17.4).

pub mod blob;
pub mod catalog;
pub mod export;
pub mod normalize;
pub mod sources;
pub mod usage;

pub use blob::{BlobStore, open_store};
pub use catalog::{Capture, PutCapture};
