//! frontmatter and body parsing. owned by the parse agent.
//! contract: `parse_document(text) -> Result<Document>`; `split_frontmatter`; `parse_body`; `render_document`.

pub mod frontmatter;
pub mod body;

pub use body::{parse_body, BodyParse};
pub use frontmatter::{render_document, split_frontmatter, parse_document};
