//! Routing: a payload that reads as one type is only offered to compressors
//! that keep what matters in it.

mod common;

use common::{collector, recoverable_text};
use kompressor_engine::{Context, EngineOptions};

/// Commit history as an MCP git server prints it: every line `Key: value`, so
/// it is detected as YAML, and a few messages carry error vocabulary.
fn mcp_git_log() -> String {
    let mut out = String::from("Commit history:\n");
    for n in (0..140).rev() {
        out.push_str(&format!(
            "Commit: {n:040x}\nAuthor: Dev {}\nDate: 2026-11-{:02} 10:00:00+00:00\n",
            n % 3,
            1 + n % 28
        ));
        if n == 90 {
            out.push_str("Message: Count events lost to a failed commit as dropped\n\n");
            out.push_str("The retry failed with an error when the pool was full.\n\n");
        } else {
            out.push_str(&format!("Message: change number {n}\n\n"));
        }
    }
    out
}

/// Config arbitration offered this payload to the whole log compressor, whose
/// error scorer kept the commit that said `failed`, the oldest commits as the
/// "tail", and put the newest behind one marker at the end — so a week-old
/// commit read as the latest. The fold that arbitration is meant to compare
/// against keeps the first of each shape, which is the head.
#[test]
fn a_commit_history_that_reads_as_config_keeps_its_newest_commit_first() {
    use kompressor_engine::pipeline::compress_content;
    let content = mcp_git_log();
    let opts = EngineOptions::default();
    let mut refs = collector(&opts);
    let ctx = Context::for_role("tool");
    let mut applied = Vec::new();
    let (out, kind) =
        compress_content(&content, "tool", &opts, None, &mut refs, &ctx, &mut applied);

    assert!(kind.is_some(), "the payload should compress");
    let newest = format!("Commit: {:040x}", 139);
    let first_commit = out.lines().find(|line| line.starts_with("Commit: "));
    assert!(
        first_commit.is_some_and(|line| line.starts_with(&newest)),
        "the newest commit must come first, got {out}"
    );
    let all = recoverable_text(&out, &refs);
    for line in content.lines().filter(|line| !line.is_empty()) {
        assert!(all.contains(line), "{line:?} was lost");
    }
}
