//! message framing for `kb mcp` over stdio: newline-delimited json (one message per line) or
//! lsp-style `Content-Length` framing, auto-detected from the first bytes of the stream. design.md
//! §18/§29 step 6 asks for both, since mcp clients differ in which one they speak.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// one json value per line, terminated by `\n`.
    Ndjson,
    /// `Content-Length: <n>\r\n\r\n<n bytes of json>`, repeated.
    ContentLength,
}

/// peek at the stream without consuming it: `Content-Length` framing starts every message with
/// that literal header line, so its presence in the first bytes is enough to tell the two apart.
/// a stream with nothing left to read (eof) defaults to ndjson, since there is nothing more to
/// frame either way.
pub fn detect_framing(reader: &mut impl BufRead) -> Result<Framing> {
    let buf = reader.fill_buf().context("peeking at the first message")?;
    if buf.starts_with(b"Content-Length") { Ok(Framing::ContentLength) } else { Ok(Framing::Ndjson) }
}

/// read one message in the given framing. `Ok(None)` at a clean eof (no message, no error).
pub fn read_message(reader: &mut impl BufRead, framing: Framing) -> Result<Option<String>> {
    match framing {
        Framing::Ndjson => read_ndjson(reader),
        Framing::ContentLength => read_content_length(reader),
    }
}

fn read_ndjson(reader: &mut impl BufRead) -> Result<Option<String>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("reading a line")?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // a client that pads its stream with blank lines between messages; skip and keep
            // reading rather than handing the dispatcher an empty body.
            continue;
        }
        return Ok(Some(trimmed.to_string()));
    }
}

fn read_content_length(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("reading a header line")?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:").or_else(|| trimmed.strip_prefix("content-length:")) {
            content_length = Some(value.trim().parse().with_context(|| format!("bad Content-Length header `{trimmed}`"))?);
        }
        // any other header (e.g. Content-Type) is read and discarded; this build only ever
        // writes Content-Length itself, but a client may send more.
    }
    let len = content_length.context("message had no Content-Length header")?;
    let mut buf = vec![0u8; len];
    std::io::Read::read_exact(reader, &mut buf).with_context(|| format!("reading {len} content-length bytes"))?;
    String::from_utf8(buf).context("content-length body was not valid utf-8").map(Some)
}

/// write one message in the given framing and flush immediately, so a stdio peer sees it right
/// away rather than waiting on an output buffer.
pub fn write_message(writer: &mut impl Write, framing: Framing, body: &str) -> Result<()> {
    match framing {
        Framing::Ndjson => {
            writeln!(writer, "{body}").context("writing an ndjson message")?;
        }
        Framing::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len()).context("writing a content-length message")?;
        }
    }
    writer.flush().context("flushing the message")
}

/// framing errors that are worth distinguishing from "the peer hung up": a malformed header, or
/// content-length bytes that never arrive.
pub fn require_message(reader: &mut impl BufRead, framing: Framing) -> Result<String> {
    match read_message(reader, framing)? {
        Some(m) => Ok(m),
        None => bail!("stream ended before a full message arrived"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn detects_ndjson_when_no_content_length_header() {
        let mut r = Cursor::new(b"{\"jsonrpc\":\"2.0\"}\n".to_vec());
        assert_eq!(detect_framing(&mut r).unwrap(), Framing::Ndjson);
    }

    #[test]
    fn detects_content_length_framing() {
        let body = "{\"jsonrpc\":\"2.0\"}";
        let mut r = Cursor::new(format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes());
        assert_eq!(detect_framing(&mut r).unwrap(), Framing::ContentLength);
    }

    #[test]
    fn ndjson_round_trip_multiple_messages() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, Framing::Ndjson, "{\"a\":1}").unwrap();
        write_message(&mut buf, Framing::Ndjson, "{\"b\":2}").unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(read_message(&mut r, Framing::Ndjson).unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(read_message(&mut r, Framing::Ndjson).unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(read_message(&mut r, Framing::Ndjson).unwrap(), None);
    }

    #[test]
    fn ndjson_skips_blank_lines_between_messages() {
        let mut r = Cursor::new(b"\n\n{\"a\":1}\n\n".to_vec());
        assert_eq!(read_message(&mut r, Framing::Ndjson).unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(read_message(&mut r, Framing::Ndjson).unwrap(), None);
    }

    #[test]
    fn content_length_round_trip_multiple_messages() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, Framing::ContentLength, "{\"a\":1}").unwrap();
        write_message(&mut buf, Framing::ContentLength, "{\"bb\":22}").unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(read_message(&mut r, Framing::ContentLength).unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(read_message(&mut r, Framing::ContentLength).unwrap().as_deref(), Some("{\"bb\":22}"));
        assert_eq!(read_message(&mut r, Framing::ContentLength).unwrap(), None);
    }

    #[test]
    fn content_length_tolerates_extra_headers() {
        let body = "{\"x\":true}";
        let msg = format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let mut r = Cursor::new(msg.into_bytes());
        assert_eq!(read_message(&mut r, Framing::ContentLength).unwrap().as_deref(), Some(body));
    }

    #[test]
    fn content_length_missing_header_errors() {
        let mut r = Cursor::new(b"Content-Type: application/json\r\n\r\nx".to_vec());
        assert!(read_message(&mut r, Framing::ContentLength).is_err());
    }

    #[test]
    fn require_message_errors_at_clean_eof() {
        let mut r = Cursor::new(Vec::new());
        assert!(require_message(&mut r, Framing::Ndjson).is_err());
    }
}
