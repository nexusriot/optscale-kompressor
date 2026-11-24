//! Tabular compression: rows are dropped whole, never cut through a record.

mod common;

use common::collector;
use kompressor_engine::tabular::crush_tabular;
use kompressor_engine::{Context, EngineOptions};

/// A quoted cell may contain a newline, so one record can span several lines.
/// Row selection works on lines; on such a table it kept half a record and
/// left a quote open. A table whose records are not its lines is declined.
#[test]
fn a_csv_with_multi_line_cells_is_not_cut_through_a_record() {
    let opts = EngineOptions::default();
    let mut content = String::from("id,note\n");
    for i in 0..60 {
        content.push_str(&format!(
            "{i},\"first half of note {i}\nsecond half of note {i}\"\n"
        ));
    }
    let mut refs = collector(&opts);
    if let Some(out) = crush_tabular(&content, &opts, &Context::default(), &mut refs) {
        assert_eq!(
            out.matches('"').count() % 2,
            0,
            "a record was cut in half, leaving a quote open:\n{out}"
        );
        assert!(
            !out.lines().any(|l| l.starts_with("second half")),
            "the tail of a record was kept without its head:\n{out}"
        );
    }
}
