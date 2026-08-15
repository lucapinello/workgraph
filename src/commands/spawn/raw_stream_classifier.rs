//! Classify an agent failure from the raw JSONL stream written by the claude/codex executors.
//!
//! This is a pure function: no side-effects, no graph I/O. The wrapper invokes
//! `wg classify-failure` which shells out to this logic.

use std::path::Path;
use worksgood::graph::FailureClass;

/// Maximum bytes to read from the tail of raw_stream.jsonl when scanning for
/// api_error_status. The relevant event is always near the end of the stream.
const TAIL_BYTES: u64 = 4096;

/// Classify an agent failure from the raw JSONL stream and exit code.
///
/// # Arguments
/// - `raw_stream`: path to the `raw_stream.jsonl` produced by the executor wrapper.
///   May not exist if the agent was killed before producing any output.
/// - `exit_code`: the shell exit code of the agent process (124 = hard timeout).
pub fn classify_from_raw_stream(raw_stream: &Path, exit_code: i32) -> FailureClass {
    // Hard timeout: exit 124 is set by the `timeout` command in the wrapper.
    if exit_code == 124 {
        return FailureClass::AgentHardTimeout;
    }

    // Read the tail of raw_stream.jsonl for api_error_status.
    let tail = match read_tail(raw_stream) {
        Some(t) => t,
        None => {
            // File missing or unreadable — could be a wrapper-internal problem
            // (exit_code != 0 with no stream) or the agent never ran.
            if exit_code != 0 {
                return FailureClass::WrapperInternal;
            }
            return FailureClass::AgentExitNonzero;
        }
    };

    // Scan for api_error_status numeric value.
    if let Some(status_code) = extract_api_error_status(&tail) {
        match status_code {
            400 => {
                // A 400 is not one thing, and this used to pretend it was: the guard below
                // "confirmed" a document error and then the fall-through returned the SAME class
                // anyway, so every 400 became api-error-400-document. On 2026-08-11 that turned
                // an exhausted API budget into "fix the malformed PDF" on task verify-next-week —
                // a document that does not exist — and, because the document class maps to
                // ProviderErrorKind::FatalTask, it marked the task permanently failed for
                // something that was not its fault. With the budget gone, every queued task would
                // have been destroyed the same way.
                //
                // Usage limit first: it is the only 400 whose right answer is "stop the run", not
                // "fix this task".
                if is_usage_limit(&tail) {
                    return FailureClass::ApiError400UsageLimit;
                }
                if tail.contains("Could not process PDF")
                    || tail.contains("Could not process document")
                    || tail.contains("Could not process image")
                {
                    return FailureClass::ApiError400Document;
                }
                // Neither — the request itself was rejected. Same conservative no-auto-retry
                // policy as a document error, but named for what we actually know.
                return FailureClass::ApiError400Other;
            }
            429 => return FailureClass::ApiError429RateLimit,
            500..=599 => return FailureClass::ApiError5xxTransient,
            _ => {}
        }
    }

    if looks_like_executor_tool_model_config_failure(&tail) {
        return FailureClass::ExecutorConfig;
    }

    FailureClass::AgentExitNonzero
}

/// Classify `NoOperationalOutput` (guardrail G4): the agent "talked but
/// didn't act" — exited cleanly / called `wg done`, produced no artifacts,
/// wrote no files outside `log/`, but left a non-empty `output.log`.
///
/// This is the *weak* fallback for tasks without a parsed `## Deliverables`
/// block (the strong signal is G1's `DeliverableMissing` preflight). When the
/// signature matches, the retry path (G3) injects the no-op directive block
/// so the loop breaks instead of repeating meta/observation work.
///
/// # Arguments
/// - `clean_exit_or_done`: exit code 0 OR the agent called `wg done`.
/// - `artifacts_empty`: `task.artifacts` is empty (no `wg artifact` calls).
/// - `has_file_writes`: files were written outside `log/` — true if
///   `git status --porcelain` is non-empty OR `output.log` shows a mutation
///   command (`write_file` / `edit_file` / `wg add` / shell-mutation). The
///   caller may derive this from either signal per the G4 rule.
/// - `output_log_nonempty`: `output.log` has non-whitespace content.
pub fn classify_no_operational_output(
    clean_exit_or_done: bool,
    artifacts_empty: bool,
    has_file_writes: bool,
    output_log_nonempty: bool,
) -> Option<FailureClass> {
    if clean_exit_or_done && artifacts_empty && !has_file_writes && output_log_nonempty {
        Some(FailureClass::NoOperationalOutput)
    } else {
        None
    }
}

/// Scan an `output.log` body for evidence of filesystem mutation — the
/// command tokens the agent shells out to write/edit files. Used by the G4
/// classifier (via the wrapper) to derive `has_file_writes` from the log
/// when `git status` is unavailable or unreliable.
///
/// Matches (case-insensitive, as substrings):
/// - `write_file` / `edit_file` — the executor tool calls.
/// - `wg add` — staging a file for commit.
/// - `wg artifact` — recording an artifact (counts as a write for G4 since
///   it implies the agent produced an output; the `artifacts_empty` signal
///   already gates this, but the log token is a corroborating signal).
/// - shell-mutation commands: `git commit`, `git mv`, `mkdir -p`, `curl`,
///   `wget`, `cp `, `mv `, `tee ` — i.e. the operational verbs an intake
///   task would use to produce its deliverables.
///
/// Returns `true` if ANY mutation token is present.
pub fn output_log_has_mutations(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    const TOKENS: &[&str] = &[
        "write_file",
        "edit_file",
        "wg add",
        "wg artifact",
        "git commit",
        "git mv",
        "mkdir -p",
        "curl ",
        "wget ",
        "cp ",
        "mv ",
        "tee ",
    ];
    TOKENS.iter().any(|t| lower.contains(t))
}

/// Read up to TAIL_BYTES from the end of `path`, returning the string content.
/// Returns None if the file doesn't exist, can't be read, or is empty.
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let offset = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    if buf.is_empty() { None } else { Some(buf) }
}

/// Extract the integer value of the first `api_error_status` key found in `text`.
/// Handles both `"api_error_status":400` and `"api_error_status": 400` (with space).
fn extract_api_error_status(text: &str) -> Option<u32> {
    let key = "api_error_status";
    let pos = text.find(key)?;
    let after = &text[pos + key.len()..];
    let mut chars = after.chars().peekable();
    // Skip closing quote (if present), then colon, then optional whitespace.
    // Input is typically: `"api_error_status":400` or `api_error_status: 400`.
    // After skipping past `api_error_status`, `after` starts with `":400` or `:400`.
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            break;
        }
        chars.next();
    }
    // read digits
    let digits: String = chars.take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Does this stream tail carry the API's "you are out of budget" 400?
///
/// The message is prose, not a code: `"You have reached your specified API usage limits. You will
/// regain access on 2026-09-01 at 00:00 UTC."` Both halves are matched independently because the
/// wording around them has changed before and the two phrases have never appeared in a
/// document-processing error.
fn is_usage_limit(tail: &str) -> bool {
    let t = tail.to_ascii_lowercase();
    t.contains("usage limit") || t.contains("regain access on")
}

fn looks_like_executor_tool_model_config_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("param: tools")
        && lower.contains("model")
        && (lower.contains("does not exist")
            || lower.contains("not found")
            || lower.contains("unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_stream(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_classifier_pdf_400_from_real_jsonl() {
        let f = write_stream(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":400,"message":"Could not process PDF"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError400Document
        );
    }

    #[test]
    fn test_classifier_pdf_400_could_not_process_document() {
        let f = write_stream(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":400,"message":"Could not process document"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError400Document
        );
    }

    #[test]
    fn test_classifier_400_usage_limit_is_not_a_document_error() {
        // The REAL payload from agent-8349 (task verify-next-week, 2026-08-11), trimmed to the
        // fields the classifier reads. It used to come back ApiError400Document, whose operator
        // hint sends you to fix a malformed PDF and whose triage kind is FatalTask — so a house
        // with no API budget left destroyed the task instead of parking the run.
        let f = write_stream(
            r#"{"type":"result","terminal_reason":"api_error","subtype":"success","api_error_status":400,"result":"API Error: 400 You have reached your specified API usage limits. You will regain access on 2026-09-01 at 00:00 UTC."}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError400UsageLimit
        );
    }

    #[test]
    fn test_classifier_400_with_no_known_cause_is_not_called_a_document_error() {
        // Behaviour CHANGE, deliberately pinned: the old code ran a "confirm it's a document
        // error" check and then returned the document class either way, so this case asserted a
        // diagnosis nobody had made.
        let f = write_stream(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":400,"message":"messages.1: all messages must have non-empty content"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError400Other
        );
    }

    #[test]
    fn test_usage_limit_detector_does_not_fire_on_a_document_error() {
        // The two 400s must stay distinguishable in both directions, or the fix just moves the
        // misclassification to the other side.
        assert!(!is_usage_limit(
            r#"{"api_error_status":400,"message":"Could not process PDF: encrypted"}"#
        ));
        assert!(is_usage_limit(
            "You have reached your specified API usage limits."
        ));
        assert!(is_usage_limit("you will regain access on 2026-09-01"));
    }

    #[test]
    fn test_classifier_429_rate_limit() {
        let f = write_stream(
            r#"{"type":"result","is_error":true,"api_error_status":429,"message":"Rate limit exceeded"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError429RateLimit
        );
    }

    #[test]
    fn test_classifier_500_transient() {
        let f = write_stream(
            r#"{"type":"result","is_error":true,"api_error_status":500,"message":"Internal server error"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError5xxTransient
        );
    }

    #[test]
    fn test_classifier_503_transient() {
        let f = write_stream(
            r#"{"type":"result","is_error":true,"api_error_status":503,"message":"Service unavailable"}"#,
        );
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError5xxTransient
        );
    }

    #[test]
    fn test_classifier_hard_timeout() {
        // File doesn't matter for exit 124
        let f = write_stream("doesn't matter");
        assert_eq!(
            classify_from_raw_stream(f.path(), 124),
            FailureClass::AgentHardTimeout
        );
    }

    #[test]
    fn test_classifier_generic_exit() {
        let f = write_stream(r#"{"type":"result","subtype":"success","result":"done"}"#);
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::AgentExitNonzero
        );
    }

    #[test]
    fn test_classifier_codex_unavailable_optional_tool_model() {
        let f = write_stream("The model 'gpt-image-2' does not exist.\nparam: tools\n");
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ExecutorConfig
        );
    }

    #[test]
    fn test_classifier_missing_raw_stream() {
        let path = std::path::PathBuf::from("/nonexistent/path/raw_stream.jsonl");
        assert_eq!(
            classify_from_raw_stream(&path, 1),
            FailureClass::WrapperInternal
        );
    }

    #[test]
    fn test_classifier_truncated_jsonl() {
        // Last line is partial JSON — should fall back, not panic
        let f = write_stream(r#"{"type":"result","api_error_status":400,"mes"#);
        // Still extracts the status code from partial JSON. The CLASS changed with the 400 split
        // (2026-08-15): a stream that stops mid-key cannot tell you why the request was rejected,
        // so calling it a document error was the same unearned diagnosis this test used to pin.
        // "Other" is what is actually known — a 400 whose cause the stream does not contain.
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::ApiError400Other
        );
    }

    #[test]
    fn test_classifier_empty_stream_nonzero_exit() {
        let f = write_stream("");
        // Empty stream + non-zero exit → WrapperInternal (no stream data)
        assert_eq!(
            classify_from_raw_stream(f.path(), 1),
            FailureClass::WrapperInternal
        );
    }

    #[test]
    fn test_extract_api_error_status_with_space() {
        assert_eq!(
            extract_api_error_status(r#""api_error_status": 400"#),
            Some(400)
        );
    }

    #[test]
    fn test_extract_api_error_status_no_space() {
        assert_eq!(
            extract_api_error_status(r#""api_error_status":429"#),
            Some(429)
        );
    }

    #[test]
    fn test_extract_api_error_status_not_found() {
        assert_eq!(extract_api_error_status(r#"{"type":"result"}"#), None);
    }

    #[test]
    fn classifier_detects_no_operational_output() {
        // Full signature: clean exit, no artifacts, no file writes, non-empty
        // output.log → NoOperationalOutput.
        assert_eq!(
            classify_no_operational_output(true, true, false, true),
            Some(FailureClass::NoOperationalOutput)
        );

        // Non-clean exit (crash/timeout) → not no-op (it's a real failure).
        assert_eq!(
            classify_no_operational_output(false, true, false, true),
            None
        );

        // Artifacts present → agent did produce something → not no-op.
        assert_eq!(
            classify_no_operational_output(true, false, false, true),
            None
        );

        // File writes detected → agent acted → not no-op.
        assert_eq!(classify_no_operational_output(true, true, true, true), None);

        // Empty output.log → crash, not meta work → not no-op.
        assert_eq!(
            classify_no_operational_output(true, true, false, false),
            None
        );
    }

    #[test]
    fn output_log_has_mutations_detects_write_tokens() {
        // Executor tool calls.
        assert!(output_log_has_mutations(
            "Used write_file to create latest.pt"
        ));
        assert!(output_log_has_mutations("edit_file src/foo.rs"));
        // wg add / artifact.
        assert!(output_log_has_mutations("ran: wg add latest.pt"));
        assert!(output_log_has_mutations("wg artifact t1 latest.pt"));
        // Shell-mutation verbs.
        assert!(output_log_has_mutations("git commit -m x"));
        assert!(output_log_has_mutations("mkdir -p seed"));
        assert!(output_log_has_mutations("curl -o x.bin URL"));
        assert!(output_log_has_mutations("cp a b"));

        // Pure meta/observation prose — no mutation tokens.
        assert!(!output_log_has_mutations(
            "Analyzed the task. The checkpoint metadata looks fine. Summary: ready."
        ));
        assert!(!output_log_has_mutations(""));
        // Case-insensitivity.
        assert!(output_log_has_mutations("WRITE_FILE latest.pt"));
    }
}
