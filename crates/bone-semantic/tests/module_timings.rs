//! A host that reports source costs asks for per-module front-end times.

use bone_semantic::prelude::*;
use std::sync::atomic::Ordering;

#[test]
fn module_timings_cover_each_module_when_requested() {
    let src = "fn twice(n: int) -> int {\n    n * 2\n}\n";
    let mut modules =
        [skull_parser::parse_module_clean(src, Ustr::from("timed.rock")).expect("parse")];
    bone_core::workspace::MODULE_TIMINGS.store(true, Ordering::Relaxed);
    let ws = semantic_pass(&mut modules);
    bone_core::workspace::MODULE_TIMINGS.store(false, Ordering::Relaxed);
    let ws = ws.expect("semantic");
    let mid = ws.module_ids[&Ustr::from("timed.rock")];
    for pass in ["lower functions", "infer functions"] {
        assert!(
            ws.module_timings
                .iter()
                .any(|(module, name, _)| *module == mid && *name == pass),
            "{pass}: {:?}",
            ws.module_timings
        );
    }
}
