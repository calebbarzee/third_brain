//! `kb provider` commands: check connectivity for a configured role, and run `classify`
//! manually against one file. owned by the provider agent; not wired into `cli/mod.rs` yet --
//! see the report that shipped with this file for the `Command` variant and dispatch snippet.

use crate::config::Config;
use crate::provider::classify::{ClassifyOutput, apply_classification, classify};
use crate::provider::{CompletionRequest, Message, Role, first_text, provider_for_role};
use anyhow::Result;
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum ProviderCommand {
    /// send a one-line completion request to the provider configured for `role` and print the reply
    Test { role: String },
    /// classify one document's layer and tags; --write fills empty fields and rewrites the file
    Classify {
        path: PathBuf,
        /// fill empty layer/tags fields in the file and rewrite it
        #[arg(long)]
        write: bool,
    },
}

pub fn run(cfg: &Config, cmd: ProviderCommand, json: bool) -> Result<i32> {
    match cmd {
        ProviderCommand::Test { role } => run_test(cfg, &role, json),
        ProviderCommand::Classify { path, write } => run_classify(cfg, &path, write, json),
    }
}

fn run_test(cfg: &Config, role: &str, json: bool) -> Result<i32> {
    let Some(provider) = provider_for_role(cfg, role)? else {
        println!("no provider configured for role \"{role}\"");
        return Ok(1);
    };
    let req = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "reply with the single word: ok".to_string() }],
        max_tokens: 16,
        temperature: Some(0.0),
        json_schema: None,
    };
    let resp = provider.complete(&req)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp.raw)?);
    } else {
        let text = first_text(&resp).unwrap_or("(no text block in response)");
        println!("provider: {}", provider.name());
        println!("reply: {text}");
    }
    Ok(0)
}

fn run_classify(cfg: &Config, path: &Path, write: bool, json: bool) -> Result<i32> {
    let text = std::fs::read_to_string(path)?;
    // a legacy `---` yaml header must be migrated to valid toml frontmatter before classifying,
    // otherwise `--write` would render a brand-new `+++` block above the untouched legacy header
    // instead of a single, valid document (design.md §6). this is migration only: it does not
    // fill in the rest of the required fields, and it never renames the file -- `check validate
    // --fix` remains the place that happens, same as it always has for a freshly classified file.
    let mut doc = match crate::check::migrate_legacy_frontmatter(&text) {
        Some((frontmatter, body)) => {
            let parsed = crate::parse::parse_body(&body);
            crate::model::Document {
                frontmatter,
                body,
                sections: parsed.sections,
                links: parsed.links,
                content_hash: crate::parse::frontmatter::sha256_hex(text.as_bytes()),
            }
        }
        None => crate::parse::parse_document(&text)?,
    };

    let Some(out) = classify(cfg, &doc, &[])? else {
        println!("no provider configured for role \"classify\"");
        return Ok(1);
    };
    print_classification(&out, json)?;

    if write {
        let filled = apply_classification(&mut doc.frontmatter, &out);
        if filled.is_empty() {
            println!("nothing to write: layer and tags were already set");
        } else {
            let rendered = crate::parse::render_document(&doc.frontmatter, &doc.body)?;
            std::fs::write(path, rendered)?;
            println!("wrote: {}", filled.join(","));
        }
    }
    Ok(0)
}

fn print_classification(out: &ClassifyOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else {
        println!("layer: {}", out.layer);
        println!("tags: {}", out.tags.join(", "));
        println!("confidence: {:.2}", out.confidence);
        println!("reason: {}", out.reason);
    }
    Ok(())
}
