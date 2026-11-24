//! Content detection: how a payload is classified decides which compressor
//! may touch it.

use kompressor_engine::detect::detect_content;

/// A payload whose lines open with a long run of `1:1:1:…` strips one gutter
/// level per recursion. Unbounded, that is a frame and a copy of the body per
/// level, and a stack overflow is an abort `catch_unwind` cannot turn into
/// "forward the original".
#[test]
fn nested_line_number_prefixes_do_not_recurse_per_level() {
    let content: String = (1..=6)
        .map(|i| format!("{}x\n", format!("{i}:").repeat(20_000)))
        .collect();
    // On a small stack, so that depth proportional to the line length cannot
    // pass by luck. One lift needs a few kilobytes; twenty thousand would not
    // fit in megabytes.
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let started = std::time::Instant::now();
            let _ = detect_content(&content);
            started.elapsed()
        })
        .unwrap();
    let elapsed = handle
        .join()
        .expect("detection must not overflow its stack");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "detection took {elapsed:?}"
    );
}
