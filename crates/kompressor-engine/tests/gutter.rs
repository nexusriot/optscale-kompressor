//! Line-number gutters: lines kept after an omission carry the numbers they
//! had in the original.

use kompressor_engine::gutter::split_line_gutter;

/// A line that survives compression must come back with *its own* number or
/// with none. Matching "the first identical line at or after the cursor" gave
/// a kept `}` the number of an earlier `}` that had been omitted.
#[test]
fn a_kept_line_never_takes_the_number_of_an_omitted_duplicate() {
    let gutter = split_line_gutter("1\talpha\n2\t}\n3\tbeta\n4\t}\n5\tgamma\n").unwrap();
    let out = gutter.reattach("alpha\n[... 2 lines omitted]\n}\ngamma\n");
    assert!(
        !out.contains("2\t}"),
        "line 4 was numbered as line 2, which was omitted:\n{out}"
    );
    assert!(out.contains("1\talpha"));
    assert!(out.contains("5\tgamma"));
    // Its true number, or bare — never a wrong one.
    assert!(out.contains("\n4\t}\n") || out.contains("\n}\n"), "{out}");
}

/// The unambiguous case still numbers every line it kept.
#[test]
fn unambiguous_lines_after_an_omission_keep_their_numbers() {
    let gutter = split_line_gutter("1\talpha\n2\tbeta\n3\tgamma\n4\tdelta\n5\tomega\n").unwrap();
    let out = gutter.reattach("alpha\n[... 2 lines omitted]\ndelta\nomega\n");
    assert_eq!(out, "1\talpha\n[... 2 lines omitted]\n4\tdelta\n5\tomega\n");
}

/// A run of identical lines kept *after* an omission is numbered from its end:
/// the lines before the marker anchor forwards, the lines after it backwards.
#[test]
fn lines_after_the_last_omission_are_numbered_from_the_end() {
    let gutter = split_line_gutter("1\t{\n2\t}\n3\t}\n4\t}\n5\t}\n6\tend\n").unwrap();
    let out = gutter.reattach("{\n[... 2 lines omitted]\n}\n}\nend\n");
    assert_eq!(out, "1\t{\n[... 2 lines omitted]\n4\t}\n5\t}\n6\tend\n");
}
