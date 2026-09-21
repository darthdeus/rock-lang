use bone_lints::{
    lint_workspace, Finding, LintOptions, HOST_RESOURCE_IN_GAME_MEMORY,
    HOST_RESOURCE_SUPPRESSION_CENSUS,
};
use bone_semantic::prelude::*;

fn findings(src: &str) -> Vec<Finding> {
    findings_for(src, HOST_RESOURCE_IN_GAME_MEMORY)
}

fn findings_for(src: &str, lint: &str) -> Vec<Finding> {
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
        &LintOptions::only([lint.to_owned()]),
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
        rock_core::errors::CompilerErrorLevel::Error,
        "game memory must reject a host handle after the finding census reaches zero"
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

#[test]
fn suppression_census_keeps_live_exceptions_visible() {
    let src = format!(
        "{HANDLE}// rock: allow(host_resource_in_game_memory, \"host-boundary fixture\")\n\
         global INTENTIONAL: Voice = Voice(0)\n\
         global UNCLASSIFIED: Voice = Voice(0)\n\
         fn main() {{ print(\"{{INTENTIONAL.0 + UNCLASSIFIED.0}}\") }}"
    );
    let census = findings_for(&src, HOST_RESOURCE_SUPPRESSION_CENSUS);
    assert_eq!(census.len(), 1, "{census:?}");
    assert!(census[0].error.message.contains("INTENTIONAL"));
    assert!(census[0].error.message.contains("host-boundary fixture"));
    let active = findings(&src);
    assert_eq!(active.len(), 1, "{active:?}");
    assert!(active[0].error.message.contains("UNCLASSIFIED"));

    let without_allow = src.replace(
        "// rock: allow(host_resource_in_game_memory, \"host-boundary fixture\")\n",
        "",
    );
    assert!(findings_for(&without_allow, HOST_RESOURCE_SUPPRESSION_CENSUS).is_empty());
    assert_eq!(findings(&without_allow).len(), 2);
}
