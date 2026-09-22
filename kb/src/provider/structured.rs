//! extract and validate a json object from a completion, retrying with the model's own text
//! plus the validation error fed back as a correction request. design.md §6, §16, §19: "use
//! native structured output where available, else a validated json block with one retry" --
//! this is that fallback path, and it is what every adapter's plain-text reply goes through.

use super::{CompletionRequest, CompletionResponse, Message, Provider, Role, first_text};
use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// the successful result of `complete_json`: the parsed value plus everything it took to get
/// there, for logging into the run log (design.md §16).
#[derive(Debug)]
pub struct Structured<T> {
    pub value: T,
    pub attempts: u32,
    pub raw_responses: Vec<CompletionResponse>,
}

/// call `provider.complete` and extract a validated `T` from the first text block, retrying up
/// to `retries` additional times with the rejection reason appended to the conversation. fails
/// with every attempt's error joined once `retries` is exhausted.
pub fn complete_json<T: DeserializeOwned>(
    provider: &dyn Provider,
    mut req: CompletionRequest,
    validate: impl Fn(&Value) -> Result<(), String>,
    retries: u32,
) -> Result<Structured<T>> {
    let mut raw_responses = Vec::new();
    let mut errors = Vec::new();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let resp = provider.complete(&req)?;
        let text = first_text(&resp).map(str::to_string);
        raw_responses.push(resp);

        match parse_and_validate::<T>(text.as_deref(), &validate) {
            Ok(value) => return Ok(Structured { value, attempts, raw_responses }),
            Err(err) => {
                errors.push(format!("attempt {attempts}: {err}"));
                if attempts > retries {
                    return Err(anyhow!("gave up after {attempts} attempt(s): {}", errors.join("; ")));
                }
                if let Some(t) = text {
                    req.messages.push(Message { role: Role::Assistant, content: t });
                }
                req.messages.push(Message {
                    role: Role::User,
                    content: format!("your previous answer was rejected: {err}. reply with only the json object."),
                });
            }
        }
    }
}

fn parse_and_validate<T: DeserializeOwned>(
    text: Option<&str>,
    validate: &impl Fn(&Value) -> Result<(), String>,
) -> Result<T, String> {
    let text = text.ok_or_else(|| "response contained no text block".to_string())?;
    let json_text =
        extract_json_object(text).ok_or_else(|| "no balanced json object found in the response text".to_string())?;
    let value: Value = serde_json::from_str(&json_text).map_err(|e| format!("invalid json: {e}"))?;
    validate(&value)?;
    serde_json::from_value::<T>(value).map_err(|e| format!("json does not match the expected shape: {e}"))
}

/// find the first balanced `{...}` object in `text`, honoring quoted strings and their escapes,
/// so a fenced code block or surrounding prose does not confuse the brace count. returns `None`
/// when no `{` is found, or the first one never balances.
pub fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    return std::str::from_utf8(&bytes[start..end]).ok().map(str::to_string);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Out {
        a: u32,
        b: String,
    }

    fn ok_validate(v: &Value) -> Result<(), String> {
        if v.get("a").is_none() {
            return Err("missing a".to_string());
        }
        Ok(())
    }

    fn base_req() -> CompletionRequest {
        CompletionRequest { system: None, messages: vec![], max_tokens: 100, temperature: None, json_schema: None }
    }

    #[test]
    fn succeeds_on_first_try() {
        let provider = FakeProvider::new(vec![r#"{"a": 1, "b": "x"}"#.to_string()]);
        let out = complete_json::<Out>(&provider, base_req(), ok_validate, 3).unwrap();
        assert_eq!(out.value, Out { a: 1, b: "x".to_string() });
        assert_eq!(out.attempts, 1);
        assert_eq!(out.raw_responses.len(), 1);
    }

    #[test]
    fn recovers_after_malformed_then_out_of_schema() {
        let provider = FakeProvider::new(vec![
            "not json at all".to_string(),
            r#"{"b": "x"}"#.to_string(), // parses, but fails validate (missing "a")
            r#"here you go: {"a": 2, "b": "y"} thanks"#.to_string(),
        ]);
        let out = complete_json::<Out>(&provider, base_req(), ok_validate, 3).unwrap();
        assert_eq!(out.value, Out { a: 2, b: "y".to_string() });
        assert_eq!(out.attempts, 3);
        assert_eq!(out.raw_responses.len(), 3);
    }

    #[test]
    fn fails_after_retries_exhausted() {
        let provider = FakeProvider::new(vec!["nope".to_string(), "still nope".to_string(), "nope again".to_string()]);
        let err = complete_json::<Out>(&provider, base_req(), ok_validate, 2).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("attempt 1"));
        assert!(msg.contains("attempt 2"));
        assert!(msg.contains("attempt 3"));
    }

    #[test]
    fn extracts_json_from_fence_and_prose() {
        let text = "sure, here is the answer:\n```json\n{\"a\": 1, \"nested\": {\"x\": \"}\\\"\"}}\n```\nhope that helps";
        let extracted = extract_json_object(text).unwrap();
        let v: Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["nested"]["x"], "}\"");
    }

    #[test]
    fn extract_json_returns_none_without_balanced_object() {
        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("{ unbalanced").is_none());
    }
}
