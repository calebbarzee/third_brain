//! shared types. additive changes only; coordinate before changing an existing field.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// abstraction level. ordered from most to least resistant to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Principle,
    Design,
    Decision,
    Procedure,
    Reference,
}

impl Layer {
    pub const ALL: [Layer; 5] = [
        Layer::Principle,
        Layer::Design,
        Layer::Decision,
        Layer::Procedure,
        Layer::Reference,
    ];

    /// rank: 0 is highest (principle). a parent must have rank <= child rank.
    pub fn rank(self) -> u8 {
        match self {
            Layer::Principle => 0,
            Layer::Design => 1,
            Layer::Decision => 2,
            Layer::Procedure => 3,
            Layer::Reference => 4,
        }
    }

    /// default agent_policy for the layer (kb.toml [layers] may override).
    pub fn default_agent_policy(self) -> AgentPolicy {
        match self {
            Layer::Principle | Layer::Design => AgentPolicy::Locked,
            Layer::Decision => AgentPolicy::AppendOnly,
            Layer::Procedure | Layer::Reference => AgentPolicy::Editable,
        }
    }

    /// default word budget (manifest.toml may override).
    pub fn default_budget_words(self) -> usize {
        match self {
            Layer::Principle => 1_000,
            Layer::Design => 4_000,
            Layer::Decision => 1_500,
            Layer::Procedure => 3_000,
            Layer::Reference => 6_000,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Principle => "principle",
            Layer::Design => "design",
            Layer::Decision => "decision",
            Layer::Procedure => "procedure",
            Layer::Reference => "reference",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Layer {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "principle" => Ok(Layer::Principle),
            "design" => Ok(Layer::Design),
            "decision" => Ok(Layer::Decision),
            "procedure" => Ok(Layer::Procedure),
            "reference" => Ok(Layer::Reference),
            other => Err(format!("unknown layer: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Draft,
    Canon,
    Superseded,
    Archived,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Draft => "draft",
            Status::Canon => "canon",
            Status::Superseded => "superseded",
            Status::Archived => "archived",
        }
    }
    /// superseded and archived documents must carry a forward pointer and a banner.
    pub fn needs_forward_pointer(self) -> bool {
        matches!(self, Status::Superseded | Status::Archived)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Status::Draft),
            "canon" => Ok(Status::Canon),
            "superseded" => Ok(Status::Superseded),
            "archived" => Ok(Status::Archived),
            other => Err(format!("unknown status: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPolicy {
    Locked,
    AppendOnly,
    Editable,
}

impl AgentPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentPolicy::Locked => "locked",
            AgentPolicy::AppendOnly => "append-only",
            AgentPolicy::Editable => "editable",
        }
    }
}

impl fmt::Display for AgentPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "locked" => Ok(AgentPolicy::Locked),
            "append-only" => Ok(AgentPolicy::AppendOnly),
            "editable" => Ok(AgentPolicy::Editable),
            other => Err(format!("unknown agent_policy: {other}")),
        }
    }
}

/// a typed edge from a document to a uri.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub rel: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// known rel values. others are accepted and warned about.
pub const REL_VALUES: [&str; 6] = ["source", "author", "code", "related", "supersedes", "asset"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Access {
    #[serde(default = "default_read")]
    pub read: Vec<String>,
    #[serde(default = "default_write")]
    pub write: Vec<String>,
}

fn default_read() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_write() -> Vec<String> {
    vec!["owner".to_string()]
}

/// optional per-document structure declaration; overrides the layer default in manifest.toml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Structure {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub critical: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

/// the frontmatter as stored in the file. every field the file may hold is here;
/// unknown fields are kept in `extra` and validated against advisory schemas as warnings.
/// required fields are Option so that a partially written file parses and `validate --fix` can fill it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<Layer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_policy: Option<AgentPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `kb:<ulid>` of the canon document one layer up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// `kb:<ulid>` or the literal `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// a date, kept as a toml datetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_by: Option<toml::value::Datetime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure: Option<Structure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<Ref>,
    /// anything not in the schema above.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

pub const REQUIRED_FIELDS: [&str; 6] = ["id", "layer", "title", "status", "agent_policy", "owner"];

impl Frontmatter {
    /// names of required fields that are missing.
    pub fn missing_required(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.id.is_none() {
            out.push("id");
        }
        if self.layer.is_none() {
            out.push("layer");
        }
        if self.title.is_none() {
            out.push("title");
        }
        if self.status.is_none() {
            out.push("status");
        }
        if self.agent_policy.is_none() {
            out.push("agent_policy");
        }
        if self.owner.is_none() {
            out.push("owner");
        }
        out
    }
}

/// a heading-bounded section of the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// slug path address, e.g. `install/linux`, `notes-2`, `_preamble`, `_body`.
    pub path: String,
    /// heading level 1..6; 0 for `_preamble` and `_body`.
    pub level: u8,
    /// heading text without the `#` marks; empty for `_preamble` and `_body`.
    pub heading: String,
    /// byte range in the body (not the whole file) covering the heading line and content,
    /// including nested child sections.
    pub start: usize,
    pub end: usize,
    /// 1-based line of the heading in the body; 1 for `_preamble` and `_body`.
    pub line: usize,
}

/// an inline link found in the body, outside code blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineLink {
    pub text: String,
    pub uri: String,
    /// true for `![alt](...)` image syntax.
    pub image: bool,
    pub line: usize,
}

/// a parsed document: frontmatter plus body plus the parsed body structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub frontmatter: Frontmatter,
    /// body text after the closing `+++` line and one newline.
    pub body: String,
    pub sections: Vec<Section>,
    pub links: Vec<InlineLink>,
    /// sha256 hex of the entire file content.
    pub content_hash: String,
}

/// the agent operation vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Create,
    Append,
    Revise,
    Rewrite,
    Supersede,
    Split,
    Merge,
    Link,
    Retire,
    Delete,
    Escalate,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Create => "create",
            Operation::Append => "append",
            Operation::Revise => "revise",
            Operation::Rewrite => "rewrite",
            Operation::Supersede => "supersede",
            Operation::Split => "split",
            Operation::Merge => "merge",
            Operation::Link => "link",
            Operation::Retire => "retire",
            Operation::Delete => "delete",
            Operation::Escalate => "escalate",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Operation {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create" => Ok(Operation::Create),
            "append" => Ok(Operation::Append),
            "revise" => Ok(Operation::Revise),
            "rewrite" => Ok(Operation::Rewrite),
            "supersede" => Ok(Operation::Supersede),
            "split" => Ok(Operation::Split),
            "merge" => Ok(Operation::Merge),
            "link" => Ok(Operation::Link),
            "retire" => Ok(Operation::Retire),
            "delete" => Ok(Operation::Delete),
            "escalate" => Ok(Operation::Escalate),
            other => Err(format!("unknown operation: {other}")),
        }
    }
}

/// merge class decided by the structure gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeClass {
    Auto,
    Review,
}

/// severity of a validate or gate finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// one finding from validate, gate, graph, or secrets checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// short kebab-case code, e.g. `missing-field`, `cycle`, `secret`, `budget`.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
}

/// parse a `kb:<ulid>[#<slug path>]` uri.
pub fn parse_kb_uri(uri: &str) -> Option<(String, Option<String>)> {
    let rest = uri.strip_prefix("kb:")?;
    let (id, frag) = match rest.split_once('#') {
        Some((a, b)) => (a, Some(b.to_string())),
        None => (rest, None),
    };
    if is_ulid(id) { Some((id.to_string(), frag)) } else { None }
}

/// crockford base32 ulid, 26 chars.
pub fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'))
}

pub fn new_ulid() -> String {
    ulid::Ulid::generate().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kb_uri_parses() {
        let (id, frag) = parse_kb_uri("kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux").unwrap();
        assert_eq!(id, "01K5A7Q2XW3ZRCM8T6YB4VJH0N");
        assert_eq!(frag.as_deref(), Some("install/linux"));
        assert!(parse_kb_uri("kb:short").is_none());
        assert!(parse_kb_uri("blob:sha256:abc").is_none());
    }

    #[test]
    fn layer_defaults() {
        assert_eq!(Layer::Design.default_agent_policy(), AgentPolicy::Locked);
        assert!(Layer::Principle.rank() < Layer::Reference.rank());
        assert_eq!("append-only".parse::<AgentPolicy>().unwrap(), AgentPolicy::AppendOnly);
    }

    #[test]
    fn ulid_roundtrip() {
        let u = new_ulid();
        assert!(is_ulid(&u));
    }
}
