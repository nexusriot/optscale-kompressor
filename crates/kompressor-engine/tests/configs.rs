//! Config compression: only comment and blank lines are elided, never text
//! that merely looks like a comment.

mod common;

use common::collector;
use kompressor_engine::configs::crush_config;
use kompressor_engine::{Context, EngineOptions};

/// A block scalar's content is literal text: a `#` line inside it is not a
/// comment. The detector looked for `key: |` and missed the list-item form
/// `- |`, which is how CI files write their scripts.
#[test]
fn a_block_scalar_in_a_list_item_is_not_read_as_comments() {
    let opts = EngineOptions {
        min_compress_chars: 0,
        ..EngineOptions::default()
    };
    let mut content =
        String::from("stages:\n  - build\nbuild:\n  image: alpine\n  script:\n    - |\n");
    for i in 0..12 {
        content.push_str(&format!(
            "      # this hash line is shell text inside the scalar, not yaml {i}\n"
        ));
    }
    content.push_str("      make all\n    - make test\ntest:\n  image: alpine\n");
    for form in ["- |", "- >-", "- |+", "- >2"] {
        let payload = content.replace("- |\n", &format!("{form}\n"));
        let mut refs = collector(&opts);
        let out = crush_config(&payload, &opts, &Context::default(), &mut refs);
        if let Some(out) = out {
            for i in 0..12 {
                assert!(
                    out.contains(&format!("not yaml {i}\n")),
                    "`{form}`: scalar line {i} was elided as a comment:\n{out}"
                );
            }
        }
    }
    // Tags, anchors and a trailing comment on the indicator line as well.
    for head in ["script: !!str |", "script: &run |", "script: | # the build"] {
        let payload = content
            .replace("  script:\n    - |\n", &format!("  {head}\n"))
            .replace("    - make test\n", "");
        let mut refs = collector(&opts);
        if let Some(out) = crush_config(&payload, &opts, &Context::default(), &mut refs) {
            assert!(
                out.contains("not yaml 5\n"),
                "`{head}`: scalar content was elided:\n{out}"
            );
        }
    }
}
