//! Log compression: what the scorer drops is announced where it was dropped.

mod common;

use common::{collector, recoverable_text};
use kompressor_engine::{Context, EngineOptions};

/// The scorer's dropped lines are announced where they stood. With one marker
/// at the end, a gap at the top of the payload was invisible.
#[test]
fn scored_log_gaps_are_marked_where_they_were() {
    use kompressor_engine::logs::{Identity, crush_log};
    let mut lines: Vec<String> = (0..120)
        .map(|n| format!("INFO step {n} processed batch of widgets"))
        .collect();
    lines[60] = "ERROR worker 7 failed: connection refused".to_string();
    let content = lines.join("\n");
    let opts = EngineOptions::default();
    let mut refs = collector(&opts);
    let out = crush_log(
        &content,
        &opts,
        &Context::default(),
        &mut refs,
        Identity::Values,
    )
    .expect("a long log with one error scores");

    let out_lines: Vec<&str> = out.lines().collect();
    let error_at = out_lines
        .iter()
        .position(|line| line.contains("ERROR worker 7"))
        .expect("the error is kept");
    assert!(
        out_lines[..error_at]
            .iter()
            .any(|line| line.contains("log lines omitted")),
        "the gap before the error must be marked before it: {out}"
    );
    assert!(
        !out_lines.last().unwrap().contains("omitted"),
        "the kept tail ends the output: {out}"
    );
    let all = recoverable_text(&out, &refs);
    for line in &lines {
        assert!(all.contains(line.as_str()), "{line:?} was lost");
    }
}
