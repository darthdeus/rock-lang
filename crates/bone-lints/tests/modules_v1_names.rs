//! Under modules v1 a definition's name is its DefKey (`<pkg>::<module>::<name>`).
//! Lints that key on a declared name (`Task`, `main`) must read the declared
//! segment, or they go silent (or noisy) the moment a package converts.

use bone_lints::{task_null_abandon, unused_fns};
use bone_semantic::prelude::*;

fn ws_v1(src: &str) -> Workspace {
    let mut module = skull_parser::parse_module_clean_with_dialect(
        src,
        Ustr::from("main.rock"),
        skull_parser::Dialect::V1,
    )
    .expect("parse");
    module.package = Some(skull_parser::PackageTag { id: 0, self_name: "lintv1".to_string() });
    semantic_pass(&mut [module]).expect("clean program typechecks")
}

#[test]
fn v1_task_null_abandon_still_fires() {
    let ws = ws_v1(
        r#"
struct Task<R = ()> { id: int }
global SLOT: Task? = null
fn abandon() { SLOT = null }
fn main() { SLOT = Task { id: 1 }; abandon() }
"#,
    );
    assert_eq!(task_null_abandon(&ws).len(), 1, "a v1 `Task?` slot set to null must be flagged");
}

#[test]
fn v1_main_is_a_reachability_root() {
    let ws = ws_v1("fn main() { used() }\nfn used() {}\nfn dead() {}");
    let mut names: Vec<String> = unused_fns(&ws).iter().map(|f| f.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["lintv1::dead".to_string()], "only `dead` is unused");
}
