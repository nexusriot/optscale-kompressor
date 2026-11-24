//! Engine-wide invariants over generated payloads: no panic, no growth, and
//! the same bytes on every run.

use kompressor_engine::{Context, EngineOptions};

/// A small deterministic generator: the cases are the same on every run and
/// on every machine, so a failure names a seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces chosen to sit on every seam the compressors cut along: each line
/// terminator `split_lines` knows, multi-byte characters, the openers of every
/// content type the detector recognises, and text that looks like a marker.
const PIECES: &[&str] = &[
    "\n",
    "\n",
    "\n",
    "\r\n",
    "\r",
    "\t",
    " ",
    "  ",
    "\x0c",
    "\x0b",
    "\u{2028}",
    "\u{85}",
    "é",
    "日本",
    "🙂",
    "{",
    "}",
    "[",
    "]",
    "\"",
    ":",
    ",",
    "```",
    "```yaml",
    "1:",
    "12\t",
    "+++",
    "---",
    "+++ b/x.rs",
    "--- a/x.rs",
    "@@ -1,3 +1,3 @@",
    "diff --git a/x b/x",
    "| a | b |",
    "|---|---|",
    "key: |",
    "- |",
    "# comment",
    "ERROR failed step ",
    "WARN retry ",
    "def f():",
    "    return 1",
    "fn x() {",
    "=> {",
    "</div>",
    "<div class=\"a\">",
    "a,b,c",
    "\"x\": 1",
    "[... 3 lines omitted]",
    "[×4]",
    "ref:k_AbC123dEf456",
    " omitted",
    "…[+12 chars]",
    "2026-11-18T10:00:00Z INFO worker ",
    "src/main.rs:12:",
    "0123456789",
    "the quick brown fox ",
];

fn payload(rng: &mut Rng) -> String {
    let pieces = 40 + rng.below(1_400);
    // A third of the cases repeat a short run, which is what makes a payload
    // compressible at all; pure noise is returned whole and proves little.
    let repeat = rng.below(3) == 0;
    let run: Vec<&str> = (0..2 + rng.below(6))
        .map(|_| PIECES[rng.below(PIECES.len())])
        .collect();
    let mut out = String::new();
    for i in 0..pieces {
        if repeat && rng.below(4) != 0 {
            out.push_str(run[i % run.len()]);
            if i % run.len() == run.len() - 1 {
                out.push_str(&(i % 97).to_string());
                out.push('\n');
            }
        } else {
            out.push_str(PIECES[rng.below(PIECES.len())]);
        }
    }
    out
}

/// The three invariants the pipeline promises for every payload, plus the one
/// `recoverable` adds, over a few hundred generated payloads and several
/// option sets:
///
/// - it never panics (a panic outside `catch_unwind` takes the request down);
/// - the output is never longer than the input;
/// - the same input gives the same output, byte for byte;
/// - every fragment stored is referenced by the output, and every handle the
///   output gained is one that was stored.
#[test]
fn generated_payloads_never_panic_grow_or_change_between_runs() {
    use kompressor_engine::pipeline::compress_content;
    use kompressor_engine::refs_in;

    let option_sets = [
        EngineOptions::default(),
        EngineOptions {
            min_compress_chars: 0,
            max_ref_bytes: 256,
            max_line_chars: 48,
            ..EngineOptions::default()
        },
        EngineOptions {
            min_compress_chars: 64,
            min_elision_saving_chars: 4_000,
            code_max_body_lines: 3,
            ..EngineOptions::default()
        },
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut compressed = 0usize;
    for case in 0..360 {
        let content = payload(&mut rng);
        let opts = &option_sets[case % option_sets.len()];
        let role = if case % 5 == 0 { "user" } else { "tool" };
        let run = |content: &str| {
            let mut refs = opts.collector();
            let mut applied = Vec::new();
            let ctx = Context::for_role(role);
            let (out, kind) =
                compress_content(content, role, opts, None, &mut refs, &ctx, &mut applied);
            (out.into_owned(), kind, refs)
        };
        let (out, kind, refs) = run(&content);
        let (again, kind_again, _) = run(&content);
        assert_eq!(out, again, "case {case}: output differs between two runs");
        assert_eq!(kind, kind_again, "case {case}");
        assert!(
            out.chars().count() <= content.chars().count(),
            "case {case}: {} -> {} characters",
            content.chars().count(),
            out.chars().count()
        );
        if kind.is_none() {
            assert_eq!(
                out, content,
                "case {case}: declined, but not returned whole"
            );
            assert!(
                refs.is_empty(),
                "case {case}: declined, but left fragments behind"
            );
            continue;
        }
        compressed += 1;
        let before = refs_in(&content);
        let after = refs_in(&out);
        for (id, _) in refs.pairs() {
            assert!(
                after.contains(id),
                "case {case}: {id} was stored and nothing in the output points at it"
            );
        }
        for id in &after {
            assert!(
                before.contains(id) || refs.pairs().iter().any(|(held, _)| held == id),
                "case {case}: the output names {id}, which was never stored"
            );
        }
    }
    assert!(
        compressed >= 30,
        "only {compressed} of 360 payloads compressed: the generator proves too little"
    );
}
