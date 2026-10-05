//! frontmatter and body parsing. owned by the parse agent.
//! contract: `parse_document(text) -> Result<Document>`; `split_frontmatter`; `parse_body`; `render_document`.

pub mod body;
pub mod frontmatter;

pub use body::{BodyParse, parse_body};
pub use frontmatter::{parse_document, render_document, split_frontmatter};
