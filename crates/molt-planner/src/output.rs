//! `read_output` (#238): an attempt reads a command's whole output, which
//! its `run` result or the done-check's feedback had to cut, through
//! `shell.output`, without running the command again.
//!
//! The shell service saves an output once it is longer than a reply
//! carries, and the result names it by id. A read takes a page from a byte
//! offset, searches for a text and says on which lines and at which offsets
//! it is, or waits for a running process's output to grow. The service
//! keeps an output to the fork and the caller that ran its command, and
//! reading it runs nothing. Offered when the planner holds `shell.output`.

use molt_api::model::ToolUse;
use molt_api::planner::tools::{self as t, ReadOutput};
use molt_api::shell::{self, OutputPage, OutputRequest, SavedOutput};
use molt_proto::{Budget, RemoteError};
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::prompts;

/// Text one read returns.
const PAGE: u32 = 16 << 10;
/// Matches one search returns.
const MATCHES: u32 = 40;
/// The longest a read may wait for a process's output.
const MAX_WAIT_S: u64 = 30;
/// Deadline of a call beyond its own wait.
const CALL_MS: u64 = 30_000;

/// The tool's definition, offered to attempts after the other built-ins.
pub(crate) fn definition() -> Value {
    json!({
        "name": t::READ_OUTPUT,
        "description": "Read the whole output of a command whose result was cut in the middle, without running it \
                        again: the result of run, of the done-check or of read_process names it by an id such as \
                        out_1a2b. Give search to find the lines that hold a text, such as error or FAILED: the \
                        result lists each with its line number and its byte offset. Give offset to read from that \
                        byte, about 16 KB at a time; the result says where the next page starts. Reading runs \
                        nothing. Output is kept for a while after its command ended and only up to a quota: the \
                        result says when part of it was not saved, and an id that expired says so.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The id the result named." },
                "offset": {
                    "type": "integer",
                    "description": "The byte to read from, or to search from. Default 0.",
                },
                "search": {
                    "type": "string",
                    "description": "Text to find, instead of reading: the lines that hold it, at most 40.",
                },
                "case_insensitive": { "type": "boolean", "description": "Search ignoring case. Default false." },
                "wait_s": {
                    "type": "integer",
                    "description": "For a process still running: seconds to wait for output past offset. Default \
                                    0, at most 30.",
                },
            },
            "required": ["id"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// `tools` with `read_output` after them.
pub(crate) fn with_outputs(mut tools: Vec<Value>) -> Vec<Value> {
    tools.push(definition());
    tools
}

pub(crate) fn parse(call: &ToolUse) -> Result<ReadOutput, String> {
    serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for {}: {e}. Check the tool's input schema and try again.", call.name))
}

pub(crate) fn detail(read: &ReadOutput) -> String {
    match &read.search {
        Some(text) => format!("search output {} for {text:?}", read.id),
        None => format!("read output {} from {}", read.id, read.offset.unwrap_or(0)),
    }
}

/// Make the read in `fork`, and the text for the model.
pub(crate) async fn run(ctx: &Ctx, fork: &str, read: ReadOutput) -> Result<String, RemoteError> {
    let wait_ms = read.wait_s.map(|s| s.min(MAX_WAIT_S) * 1000).filter(|&ms| ms > 0);
    let req = OutputRequest {
        workspace: fork.to_owned(),
        id: read.id,
        offset: read.offset,
        limit: Some(PAGE),
        search: read.search.filter(|s| !s.is_empty()),
        case_insensitive: read.case_insensitive.unwrap_or(false),
        max_matches: Some(MATCHES),
        wait_ms,
    };
    let searched = req.search.clone();
    let budget = Budget::new(0, wait_ms.unwrap_or(0) + CALL_MS, 0);
    let page: OutputPage = ctx.call(shell::OUTPUT, req, budget).await?;
    Ok(show(&page, searched.as_deref()))
}

/// What a saved output is, in one sentence.
fn about(o: &SavedOutput) -> String {
    let command = prompts::head(o.command.lines().next().unwrap_or(""), 80);
    let mut out = format!("Output {} of `{command}` ({}): ", o.id, o.stream.name());
    if o.whole() {
        out.push_str(&format!("{} bytes, all saved", o.total));
    } else {
        out.push_str(&format!("{} bytes printed, the first {} saved", o.total, o.stored));
        if let Some(cut) = o.cut {
            out.push_str(&format!("; the rest was not saved: {}", cut.describe()));
        }
    }
    if o.open {
        out.push_str("; the process runs on and it may grow.");
    } else {
        match o.end {
            Some(end) if end.timed_out => out.push_str("; the command timed out."),
            Some(end) if end.cancelled => out.push_str("; the command was cancelled or stopped."),
            Some(end) => match (end.exit_code, end.signal) {
                (Some(code), _) => out.push_str(&format!("; the command exited with code {code}.")),
                (None, Some(signal)) => out.push_str(&format!("; the command was killed by signal {signal}.")),
                (None, None) => out.push('.'),
            },
            None => out.push_str("; how the command ended is not known."),
        }
    }
    out
}

fn show(page: &OutputPage, searched: Option<&str>) -> String {
    let mut out = about(&page.output);
    match searched {
        Some(text) if page.matches.is_empty() => {
            out.push_str(&format!("\nNo saved line from byte {} holds {text:?}.", page.offset));
        }
        Some(text) => {
            out.push_str(&format!("\nLines from byte {} that hold {text:?}:", page.offset));
            for m in &page.matches {
                out.push_str(&format!("\nline {} (offset {}): {}", m.line, m.offset, m.text));
            }
            if page.more {
                out.push_str(&format!("\n[more may follow: search again with offset {}]", page.next));
            }
        }
        None if page.text.is_empty() => out.push_str(&format!("\n(nothing saved past byte {})", page.offset)),
        None => {
            out.push_str(&format!("\nBytes {} to {}:\n{}", page.offset, page.next, page.text));
            if page.next < page.output.stored {
                out.push_str(&format!("\n[continues: read again with offset {}]", page.next));
            }
        }
    }
    out
}

/// The note a result that was cut ends with: where its whole output is,
/// and that the part not saved cannot be read.
pub(crate) fn mention(saved: &[SavedOutput], cut: bool) -> String {
    if !cut {
        return String::new();
    }
    if saved.is_empty() {
        return "\n[The part cut from the middle was not saved: run the command again, with less output, to see it]"
            .to_owned();
    }
    let mut out = String::new();
    for o in saved {
        let kept = if o.whole() {
            format!("all {} bytes", o.total)
        } else {
            format!("the first {} of {} bytes; the rest was not saved", o.stored, o.total)
        };
        out.push_str(&format!(
            "\n[The whole {} is saved as {} ({kept}): use read_output to search it or read it by offset, without \
             running the command again]",
            o.stream.name(),
            o.id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use molt_api::shell::{OutputCut, OutputEnd, OutputMatch, OutputStream};

    use super::*;

    /// The tool's definition as the model API gets it, one line of JSON.
    const PINNED: &str = include_str!("testdata/read_output_tool.txt");

    fn saved() -> SavedOutput {
        SavedOutput {
            id: "out_1".into(),
            stream: OutputStream::Stdout,
            command: "cargo test\nmore".into(),
            total: 900_000,
            stored: 900_000,
            cut: None,
            open: false,
            end: Some(OutputEnd { exit_code: Some(101), ..OutputEnd::default() }),
            sha256: Some("ab".into()),
            trace: None,
            request: None,
            kept_s: 7200,
        }
    }

    #[test]
    fn the_read_output_tool_is_pinned_strict_and_closed() {
        let def = definition();
        assert_eq!(serde_json::to_string(&def).unwrap(), PINNED.trim_end());
        assert_eq!(def["strict"], true);
        assert_eq!(def["input_schema"]["additionalProperties"], false);
        let before = vec![json!({ "name": "read_file" })];
        assert_eq!(with_outputs(before.clone())[..1], before[..]);
    }

    #[test]
    fn a_search_says_where_each_line_is_and_a_page_where_the_next_starts() {
        let found = OutputPage {
            output: saved(),
            offset: 0,
            next: 500_000,
            text: String::new(),
            matches: vec![OutputMatch { line: 4012, offset: 451_200, text: "error[E0308]: mismatched types".into() }],
            more: true,
        };
        assert_eq!(
            show(&found, Some("error")),
            "Output out_1 of `cargo test` (stdout): 900000 bytes, all saved; the command exited with code 101.\n\
             Lines from byte 0 that hold \"error\":\nline 4012 (offset 451200): error[E0308]: mismatched types\n\
             [more may follow: search again with offset 500000]"
        );
        let cut = SavedOutput { stored: 1000, cut: Some(OutputCut::QuotaFull), ..saved() };
        let page = OutputPage { output: cut, offset: 0, next: 4, text: "abcd".into(), matches: vec![], more: false };
        let shown = show(&page, None);
        assert!(shown.contains("the first 1000 saved; the rest was not saved: the shell service's saved outputs reached their disk quota"), "{shown}");
        assert!(shown.ends_with("Bytes 0 to 4:\nabcd\n[continues: read again with offset 4]"), "{shown}");
    }

    #[test]
    fn a_cut_result_names_its_saved_output_or_says_it_was_not_saved() {
        assert_eq!(mention(&[saved()], false), "");
        let named = mention(&[saved()], true);
        assert!(named.contains("The whole stdout is saved as out_1 (all 900000 bytes)"), "{named}");
        assert!(mention(&[], true).contains("was not saved"));
    }
}
