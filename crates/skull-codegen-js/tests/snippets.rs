//! Static checks on the JS intrinsic table: distinct keys, and every entry
//! backed by a stdlib `@intrinsic` declaration (stdlib or the test-local host
//! fixtures). What each mapping computes runs under node in the corpus
//! (`tests/intrinsics/`, `tests/native_buffers/`).

use skull_codegen_js::{JS_INTRINSICS, JsIntrinsics};
use skull_mir::intrinsics::{declared_intrinsics, unbacked_entries};

// `JsIntrinsics::lookup` takes the first matching row, so a duplicate key
// would silently shadow its twin.
#[test]
fn table_entries_are_distinct_keys() {
    let mut keys: Vec<_> = JS_INTRINSICS.iter().map(|(n, a, _)| (*n, *a)).collect();
    let total = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "duplicate (name, arity) in JS table");
}

// Every JS table key must have a matching declaration in the stdlib or the
// corpus put/try_call declarations. A source rename or arity drift fails here.
#[test]
fn every_js_entry_is_backed_by_a_declared_intrinsic() {
    let modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    let mut modules = modules;
    modules.push(
        skull_parser::parse_module_clean(
            include_str!("../../../tests/library/js_put.rock"),
            ustr::Ustr::from("<test>"),
        )
        .expect("parse"),
    );
    modules.push(
        skull_parser::parse_module_clean(
            include_str!("../../../tests/library/js_try_call.rock"),
            ustr::Ustr::from("<try-call>"),
        )
        .expect("parse try_call fixture"),
    );
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");

    let declared = declared_intrinsics(&ws);
    let unbacked = unbacked_entries(&JsIntrinsics(&ws), &declared);
    assert!(
        unbacked.is_empty(),
        "JS table has entries no @intrinsic declaration backs (drift): {unbacked:?}"
    );
}
