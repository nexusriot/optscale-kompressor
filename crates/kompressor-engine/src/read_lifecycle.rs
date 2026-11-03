//! Which file Reads in a conversation are still showing what is on disk.
//!
//! An agent reads a file, edits it, and then carries the pre-edit bytes in its
//! context for the rest of the session — often the largest single payload there
//! and, worse, a wrong one: the model reasons about lines that no longer exist.
//! Replacing a superseded Read with a marker is therefore a correction as much
//! as a saving.
//!
//! The classification is purely positional: it reads tool names and paths out
//! of the message list and touches no filesystem. That keeps it deterministic
//! and correct in the case that matters, a sandbox whose files the service
//! cannot see at all.
//!
//! # It is deterministic but **not** prefix-monotonic
//!
//! An earlier version of this comment claimed the determinism was "what the
//! provider prompt cache depends on". It is not, and the two must not be
//! conflated. Determinism is *same input, same output*. What a prompt cache
//! depends on is the stronger property `crossturn` states and every other
//! decision in this engine respects: the output of messages `0..k` is
//! byte-identical whether or not message `k+1` exists.
//!
//! This module is the one place that breaks it. Marking a read `Stale` requires
//! a *later* write, so appending a turn rewrites an earlier turn — and a
//! rewrite inside a prefix the provider has already cached costs a re-read of
//! everything after it. That is why `proxy.frozen_prefix = "first_seen"`, which
//! lowers the frozen floor precisely so history can be compressed, withholds
//! this feature: at Opus rates, re-writing a 20,000-token suffix to remove a
//! 2,000-token stale read needs ~125 further turns to break even, against
//! sessions that run 20 to 70.
//!
//! The replacement in `messages::run` handles `MessageContent::Text` only, so
//! it cannot fire for tool results that walk to `Blocks` — which is most of
//! Anthropic's. The reach is narrower than the rule suggests.

use std::collections::BTreeMap;

use serde::Serialize;

/// Whether a Read still reflects the file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadState {
    Fresh,
    /// A later tool call in the same conversation wrote to the same path.
    Stale,
}

impl ReadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadState::Fresh => "fresh",
            ReadState::Stale => "stale",
        }
    }
}

/// The per-message metadata the classifier needs, so it does not depend on the
/// full message type.
pub struct ReadMessage<'a> {
    pub tool_name: Option<&'a str>,
    pub path: Option<&'a str>,
    pub protected: bool,
    /// The tool reported a failure, so a write here changed nothing.
    pub failed: bool,
}

/// Tools whose result is a file's contents.
const READ_TOOLS: [&str; 2] = ["read", "read_file"];
/// Tools that change a file's contents.
const WRITE_TOOLS: [&str; 4] = ["edit", "write", "write_file", "apply_patch"];

/// How a write tool's result opens when the write did not happen.
///
/// The OpenAI families carry no error flag on a tool result, so the result's
/// own first line is the only evidence. Claude Code's `<tool_use_error>` and
/// codex's `apply_patch verification failed` are the spellings measured; the
/// rest are the generic ones harnesses prefix an exception with.
const FAILURE_PREFIXES: [&str; 6] = [
    "error",
    "<tool_use_error>",
    "failed",
    "failure",
    "fatal",
    "apply_patch verification failed",
];

/// Whether a tool result says the call failed.
///
/// Read off the first non-blank line only: a successful write that echoes
/// file content containing the word "error" further down is still a write.
pub fn reports_failure(result: &str) -> bool {
    let Some(first) = result.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return false;
    };
    let first = first.to_ascii_lowercase();
    FAILURE_PREFIXES
        .iter()
        .any(|prefix| first.starts_with(prefix))
}

/// Compare two paths the way an agent harness spells them.
///
/// The same file arrives as `./src/a.rs`, `src/a.rs`, `src\a.rs` and
/// `src//a.rs` within one conversation, depending on which tool reported it, so
/// a byte comparison would miss most write-after-read pairs.
///
/// Only spellings of the same path are folded: separators, empty components
/// and `.` components. A leading dot is part of a name (`.env` is not `env`),
/// `..` leaves the directory, and a leading `/` is the root, not the working
/// directory — stripping any of those made a write to one file mark a read of
/// another stale. `..` is kept rather than resolved, because through a symlink
/// `a/..` is not where `a` came from.
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Classify every Read in `messages` as fresh or stale.
///
/// The returned map holds only Read messages, keyed by their index.
///
/// A Read inside the frozen prefix, or one the caller flagged protected, stays
/// fresh no matter what follows it: those bytes are what the provider's prompt
/// cache is holding, and rewriting them invalidates that prefix and everything
/// after it — a cache miss on the whole conversation costs far more than the
/// stale payload does.
pub fn classify_reads(
    messages: &[ReadMessage],
    frozen_message_count: usize,
) -> BTreeMap<usize, ReadState> {
    let mut reads: Vec<(usize, String, bool)> = Vec::new();
    let mut writes: Vec<(usize, String)> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let Some(path) = message.path else { continue };
        let tool = message.tool_name.unwrap_or("").to_ascii_lowercase();
        let path = normalize_path(path);
        if READ_TOOLS.contains(&tool.as_str()) {
            let protected = index < frozen_message_count || message.protected;
            reads.push((index, path, protected));
        } else if WRITE_TOOLS.contains(&tool.as_str()) && !message.failed {
            // A failed write left the file as it was: the read before it is
            // still the truth, and the one the agent needs to retry.
            writes.push((index, path));
        }
    }

    reads
        .into_iter()
        .map(|(index, path, protected)| {
            let stale = !protected
                && writes
                    .iter()
                    .any(|(write_index, write_path)| *write_index > index && *write_path == path);
            (
                index,
                if stale {
                    ReadState::Stale
                } else {
                    ReadState::Fresh
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg<'a>(tool: &'a str, path: &'a str) -> ReadMessage<'a> {
        ReadMessage {
            tool_name: Some(tool),
            path: Some(path),
            protected: false,
            failed: false,
        }
    }

    /// An ordinary conversation turn: no tool, no path.
    fn plain<'a>() -> ReadMessage<'a> {
        ReadMessage {
            tool_name: None,
            path: None,
            protected: false,
            failed: false,
        }
    }

    #[test]
    fn a_read_followed_by_a_write_to_the_same_file_is_stale() {
        let messages = [msg("Read", "src/a.rs"), msg("Edit", "src/a.rs")];
        let states = classify_reads(&messages, 0);
        assert_eq!(states.get(&0), Some(&ReadState::Stale));
        assert_eq!(states.len(), 1, "only reads are classified");
    }

    #[test]
    fn a_write_before_the_read_leaves_it_fresh() {
        let messages = [msg("Write", "src/a.rs"), msg("Read", "src/a.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&1),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn a_write_to_a_different_file_leaves_the_read_fresh() {
        let messages = [msg("read", "src/a.rs"), msg("edit", "src/b.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn every_writing_tool_invalidates_a_read() {
        for tool in ["edit", "write", "write_file", "apply_patch", "Apply_Patch"] {
            let messages = [msg("read_file", "a.rs"), msg(tool, "a.rs")];
            assert_eq!(
                classify_reads(&messages, 0).get(&0),
                Some(&ReadState::Stale),
                "{tool} should invalidate"
            );
        }
    }

    #[test]
    fn a_tool_that_is_neither_read_nor_write_is_ignored() {
        let messages = [msg("Bash", "src/a.rs"), msg("grep", "src/a.rs")];
        assert!(classify_reads(&messages, 0).is_empty());
    }

    #[test]
    fn a_message_without_a_path_is_ignored() {
        let messages = [
            plain(),
            ReadMessage {
                tool_name: Some("read"),
                path: None,
                protected: false,
                failed: false,
            },
        ];
        assert!(classify_reads(&messages, 0).is_empty());
    }

    #[test]
    fn a_read_inside_the_frozen_prefix_is_never_rewritten() {
        let messages = [msg("read", "src/a.rs"), msg("edit", "src/a.rs")];
        // index 0 is inside the first message of the frozen prefix
        assert_eq!(
            classify_reads(&messages, 1).get(&0),
            Some(&ReadState::Fresh)
        );
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Stale)
        );
    }

    #[test]
    fn a_protected_read_is_never_rewritten() {
        let messages = [
            ReadMessage {
                tool_name: Some("read"),
                path: Some("src/a.rs"),
                protected: true,
                failed: false,
            },
            msg("edit", "src/a.rs"),
        ];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn the_same_file_spelled_differently_is_still_the_same_file() {
        let messages = [msg("Read", "./src/a.rs"), msg("Edit", "src\\a.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Stale)
        );
    }

    #[test]
    fn only_the_reads_before_the_write_go_stale() {
        let messages = [
            msg("read", "a.rs"),
            msg("edit", "a.rs"),
            msg("read", "a.rs"),
        ];
        let states = classify_reads(&messages, 0);
        assert_eq!(states.get(&0), Some(&ReadState::Stale));
        assert_eq!(
            states.get(&2),
            Some(&ReadState::Fresh),
            "the re-read is current"
        );
    }

    #[test]
    fn normalisation_folds_the_spellings_a_harness_produces() {
        assert_eq!(normalize_path("src\\a.rs"), "src/a.rs");
        assert_eq!(normalize_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("././src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src//a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src/./a.rs"), "src/a.rs");
        assert_eq!(normalize_path("/src//a.rs"), "/src/a.rs");
        assert_eq!(normalize_path("src/a.rs"), "src/a.rs");
    }

    #[test]
    fn normalisation_keeps_what_names_a_different_file() {
        // A leading dot is part of the name, `..` leaves the directory, and a
        // leading slash is the root rather than the working directory.
        assert_eq!(normalize_path(".env"), ".env");
        assert_eq!(normalize_path("./.env"), ".env");
        assert_eq!(normalize_path("../src/a.rs"), "../src/a.rs");
        assert_eq!(normalize_path("/etc/config"), "/etc/config");
        // Not resolved: through a symlink, `a/..` is not where `a` came from.
        assert_eq!(normalize_path("a/../b.rs"), "a/../b.rs");
    }

    #[test]
    fn a_write_to_a_different_file_with_a_similar_spelling_leaves_the_read_fresh() {
        for (read, write) in [
            (".env", "env"),
            ("src/a.rs", "../src/a.rs"),
            ("/etc/config", "etc/config"),
        ] {
            let messages = [msg("Read", read), msg("Write", write)];
            assert_eq!(
                classify_reads(&messages, 0).get(&0),
                Some(&ReadState::Fresh),
                "a write to {write} says nothing about {read}"
            );
        }
    }

    #[test]
    fn a_failed_write_leaves_the_read_fresh() {
        let messages = [
            msg("read", "src/a.rs"),
            ReadMessage {
                tool_name: Some("edit"),
                path: Some("src/a.rs"),
                protected: false,
                failed: true,
            },
        ];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn a_failure_is_read_off_the_first_line_only() {
        assert!(reports_failure("ERROR: permission denied; file unchanged"));
        assert!(reports_failure(
            "\n  <tool_use_error>File has not been read yet."
        ));
        assert!(reports_failure(
            "apply_patch verification failed: no such hunk"
        ));
        assert!(!reports_failure(
            "The file a.rs has been updated.\nerror: kept"
        ));
        assert!(!reports_failure("ok"));
        assert!(!reports_failure(""));
    }

    #[test]
    fn classification_is_stable_across_runs() {
        let messages = [
            msg("read", "a.rs"),
            msg("read", "b.rs"),
            msg("edit", "b.rs"),
        ];
        let once = classify_reads(&messages, 0);
        let twice = classify_reads(&messages, 0);
        assert_eq!(once, twice);
        assert_eq!(
            once.keys().copied().collect::<Vec<_>>(),
            vec![0, 1],
            "keys come back in message order"
        );
    }
}
