//! Mixed content: a segment compressed inside a larger payload leaves the
//! surrounding structure intact.

mod common;

use common::collector;
use kompressor_engine::mixed::crush_mixed;
use kompressor_engine::{Context, EngineOptions};

/// Compressing the body of a fenced block must leave the closing fence on a
/// line of its own. The body capture includes the newline before the fence and
/// the compressors return text without one, so the fence was glued to the last
/// line of the body and the block never closed.
#[test]
fn a_compressed_fenced_block_still_closes() {
    let opts = EngineOptions::default();
    let mut body = String::new();
    for i in 0..400 {
        body.push_str(&format!(
            "2026-11-18T10:00:{:02}Z INFO worker processed batch {i} in {}ms\n",
            i % 60,
            10 + i % 7
        ));
    }
    let content = format!("Here is the run:\n\n```\n{body}```\n\nThat is all of it.\n");
    let mut refs = collector(&opts);
    let Some((out, _)) = crush_mixed(&content, &opts, &Context::for_role("tool"), &mut refs) else {
        return;
    };
    assert!(out.len() < content.len());
    assert!(out.starts_with("Here is the run:\n\n```\n"), "{out}");
    assert!(
        out.ends_with("\n```\n\nThat is all of it.\n"),
        "the closing fence is not on its own line:\n…{}",
        &out[out.len().saturating_sub(160)..]
    );
    assert_eq!(out.matches("```").count(), 2);
}
