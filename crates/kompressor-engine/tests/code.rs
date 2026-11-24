//! Code compression: an elided body is stored whole, and nothing outside it is
//! dropped or altered.

#![cfg(feature = "code-aware")]

mod common;

use common::{collector, recoverable_text};
use kompressor_engine::code::crush_code;
use kompressor_engine::{Context, EngineOptions, Language};

/// An arrow function whose body is an *expression* that happens to contain
/// braces (a curried thunk) is not a block. The body node was replaced whole
/// while only the text between its first `{` and last `}` was stored, so
/// `async (dispatch) =>` was deleted and recoverable from nowhere.
#[test]
fn an_expression_bodied_arrow_function_loses_nothing() {
    let opts = EngineOptions::default();
    let mut src = String::from("export const fetchUser = (id) => async (dispatch) => {\n");
    for i in 0..60 {
        src.push_str(&format!("  dispatch({{ type: 'step{i}', id }});\n"));
    }
    src.push_str("};\n");
    let mut refs = collector(&opts);
    let out = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::TypeScript),
    );
    let Some(out) = out else {
        return; // declining is always allowed
    };
    let all = recoverable_text(&out, &refs);
    assert!(
        all.contains("async (dispatch) =>"),
        "the inner arrow head is in neither the output nor a fragment:\n{out}"
    );
    for i in [0, 30, 59] {
        assert!(all.contains(&format!("'step{i}'")), "step{i} is gone");
    }
}

/// The same for a call wrapped round an object literal: `foo(a, {…}, c)`.
#[test]
fn an_arrow_function_returning_a_call_keeps_the_call() {
    let opts = EngineOptions::default();
    let mut src = String::from("export const build = (a, c) => register(a, {\n");
    for i in 0..60 {
        src.push_str(&format!("  key{i}: compute(a, {i}),\n"));
    }
    src.push_str("}, c);\n");
    let mut refs = collector(&opts);
    let Some(out) = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::TypeScript),
    ) else {
        return;
    };
    let all = recoverable_text(&out, &refs);
    assert!(
        all.contains("register(a, {"),
        "the call head is gone:\n{out}"
    );
    assert!(all.contains("}, c);"), "the call tail is gone:\n{out}");
}

/// A form feed is a line break to `split_lines` and not to tree-sitter, whose
/// rows count `\n` only. Each one before a function shifted the elided span up
/// a line: the `def` went behind the marker and the last body line stayed.
#[test]
fn a_form_feed_before_a_python_function_does_not_shift_what_is_elided() {
    let opts = EngineOptions::default();
    let mut src = String::from("def prev():\n    return 1\n\x0c\ndef target():\n");
    for i in 0..40 {
        src.push_str(&format!("    x{i} = compute({i})\n"));
    }
    src.push_str("    return x0\n");
    let mut refs = collector(&opts);
    let Some(out) = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::Python),
    ) else {
        return;
    };
    assert!(
        out.contains("def target():"),
        "the signature was elided:\n{out}"
    );
    assert!(
        !out.contains("\n    return x0"),
        "the last body line was left outside the elision:\n{out}"
    );
    let all = recoverable_text(&out, &refs);
    assert!(all.contains("    return x0"));
    assert!(all.contains("    x0 = compute(0)"));
}
