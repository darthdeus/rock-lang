use bone_lints::{lint_workspace, Finding, LintOptions, HOST_RESOURCE_IN_GAME_MEMORY};
use bone_semantic::prelude::*;

fn findings(src: &str) -> Vec<Finding> {
    let file = "game/main.rock";
    let ast = skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse");
    let ws = semantic_pass(&mut [ast]).expect("clean program typechecks");
    let mut sources = FxHashMap::default();
    sources.insert(Ustr::from(file), src);
    lint_workspace(
        &ws,
        &FxHashSet::default(),
        true,
        &sources,
        &LintOptions::only([HOST_RESOURCE_IN_GAME_MEMORY.to_owned()]),
    )
}

const HANDLE: &str = "@host_resource(voice)\nnewtype Voice(int)\n";

#[test]
fn a_bare_handle_global_in_game_memory_is_rejected() {
    let found = findings(&format!("{HANDLE}global LIVE: Voice = Voice(0)\nfn main() {{ print(\"{{LIVE.0}}\") }}"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].error.message.contains("`LIVE`"), "{}", found[0].error.message);
    assert!(found[0].error.message.contains("`Voice`"), "{}", found[0].error.message);
    assert_eq!(
        found[0].level(),
        rock_core::errors::CompilerErrorLevel::Warning,
        "Warning, not Error, because the tree-wide census is 251 declarations \
         (docs/state-ownership-plan.md S2); raising it is one line once that is worked down"
    );
}

#[test]
fn a_handle_behind_a_struct_field_names_the_path() {
    let src = format!(
        "{HANDLE}struct Slot {{ v: Voice, n: int }}\nglobal SLOTS: [Slot] = []\n\
         fn main() {{ print(\"{{SLOTS.len()}}\") }}"
    );
    let found = findings(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    let message = &found[0].error.message;
    assert!(message.contains("Slot.v"), "path must name the field: {message}");
    assert!(message.contains("[Slot]"), "path must name the container: {message}");
}

#[test]
fn a_handle_behind_a_nullable_and_a_map_value_is_reached() {
    let src = format!(
        "{HANDLE}struct Slot {{ v: Voice? }}\nglobal BY_KEY: Map<string, Slot> = map_new()\n\
         fn main() {{ print(\"{{BY_KEY.len()}}\") }}"
    );
    let found = findings(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].error.message.contains("Slot.v"), "{}", found[0].error.message);
}

#[test]
fn a_handle_in_an_enum_payload_is_reached() {
    let src = format!(
        "{HANDLE}enum Sfx {{ Silent, Playing(Voice) }}\nglobal CURRENT: Sfx = Sfx::Silent\n\
         fn main() {{ print(\"{{match CURRENT {{ Sfx::Silent => 0, Sfx::Playing(v) => v.0 }}}}\") }}"
    );
    let found = findings(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].error.message.contains("Sfx::Playing"), "{}", found[0].error.message);
}

#[test]
fn platform_memory_and_const_are_both_accepted() {
    let src = format!(
        "{HANDLE}@state(ownership=host_projection, world=exclude, realization=rebuild_host_projection)\n\
         global OK: Voice = Voice(0)\nconst FROZEN: Voice = Voice(0)\n\
         fn main() {{ print(\"{{OK.0 + FROZEN.0}}\") }}"
    );
    assert!(findings(&src).is_empty(), "{:?}", findings(&src));
}

#[test]
fn a_memory_platform_header_accepts_the_whole_file() {
    let src = format!(
        "@memory(platform)\n{HANDLE}global LIVE: Voice = Voice(0)\nfn main() {{ print(\"{{LIVE.0}}\") }}"
    );
    assert!(findings(&src).is_empty(), "{:?}", findings(&src));
}

#[test]
fn an_unannotated_type_over_an_int_is_not_a_host_resource() {
    let src = "newtype MobId(int)\nglobal TARGET: MobId = MobId(0)\nfn main() { print(\"{TARGET.0}\") }";
    assert!(findings(src).is_empty(), "{:?}", findings(src));
}

#[test]
fn a_recursive_struct_terminates() {
    let src = format!(
        "{HANDLE}struct Node {{ next: Node?, v: Voice }}\nglobal ROOT: Node? = null\n\
         fn main() {{ print(\"{{ROOT == null}}\") }}"
    );
    let found = findings(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].error.message.contains("Node.v"), "{}", found[0].error.message);
}

#[test]
fn a_recursive_struct_with_no_handle_terminates_without_a_finding() {
    let src = "struct Node { next: Node?, n: int }\nglobal ROOT: Node? = null\n\
               fn main() { print(\"{ROOT == null}\") }";
    assert!(findings(src).is_empty(), "{:?}", findings(src));
}

#[test]
fn an_allow_comment_is_the_documented_temporary_escape() {
    let src = format!(
        "{HANDLE}// rock: allow(host_resource_in_game_memory)\nglobal LIVE: Voice = Voice(0)\n\
         fn main() {{ print(\"{{LIVE.0}}\") }}"
    );
    assert!(findings(&src).is_empty(), "{:?}", findings(&src));
}
