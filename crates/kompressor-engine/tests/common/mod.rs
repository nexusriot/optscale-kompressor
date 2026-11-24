//! Helpers shared by the engine's integration tests.

#![allow(dead_code)]

use kompressor_engine::{EngineOptions, RefCollector};

pub fn collector(opts: &EngineOptions) -> RefCollector {
    opts.collector()
}

/// Everything the engine kept or stored: the output and every fragment.
pub fn recoverable_text(out: &str, refs: &RefCollector) -> String {
    let mut all = out.to_string();
    for (_, fragment) in refs.pairs() {
        all.push('\n');
        all.push_str(fragment);
    }
    all
}
