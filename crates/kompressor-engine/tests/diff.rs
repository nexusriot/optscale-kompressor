//! Diff compression: every changed line is kept or stored, whatever it
//! begins with.

mod common;

use common::collector;
use kompressor_engine::diff::crush_diff;
use kompressor_engine::{Context, EngineOptions};

/// Inside a hunk every line that starts with `+` or `-` is a change, whatever
/// follows: `+++counter;` adds the C statement `++counter;`. It was taken for
/// neither a change nor a header and trimmed away as "context".
#[test]
fn an_added_line_that_begins_with_plus_plus_is_a_change() {
    let opts = EngineOptions::default();
    let mut diff = String::new();
    diff.push_str("diff --git a/src/count.c b/src/count.c\n--- a/src/count.c\n+++ b/src/count.c\n");
    diff.push_str("@@ -1,40 +1,42 @@\n");
    for i in 0..20 {
        diff.push_str(&format!(" int unchanged_before_{i} = {i};\n"));
    }
    diff.push_str("+++counter;\n");
    diff.push_str("---legacy_counter;\n");
    for i in 0..20 {
        diff.push_str(&format!(" int unchanged_after_{i} = {i};\n"));
    }
    diff.push_str("diff --git a/src/other.c b/src/other.c\n--- a/src/other.c\n+++ b/src/other.c\n");
    diff.push_str("@@ -1,30 +1,31 @@\n");
    for i in 0..15 {
        diff.push_str(&format!(" int other_before_{i} = {i};\n"));
    }
    diff.push_str("+int added = 1;\n");
    for i in 0..15 {
        diff.push_str(&format!(" int other_after_{i} = {i};\n"));
    }
    let mut refs = collector(&opts);
    let Some(out) = crush_diff(&diff, &opts, &Context::default(), &mut refs) else {
        return;
    };
    assert!(out.len() < diff.len());
    assert!(
        out.contains("\n+++counter;\n"),
        "the added line was dropped:\n{out}"
    );
    assert!(
        out.contains("\n---legacy_counter;\n"),
        "the removed line was dropped:\n{out}"
    );
    assert!(out.contains("\n+int added = 1;\n"));
    // The file headers are still headers.
    assert!(out.contains("\n+++ b/src/count.c\n"));
}
