//! The full Artifact binding set is compiler data, not a runtime scan of `_G`.
//! These tests are deliberately expressed through the emitted publication plan
//! so ordinary and shadow publication have one observable membership contract.

mod common;

use common::{emit_mir, run_luajit};
use skull_mir::artifact_plan::{ArtifactBinding, ArtifactBindingKind, ArtifactBindingPlan};

fn plan_snapshot(lua: &str, label: &str) -> String {
    run_luajit(
        &format!(
            r#"
{lua}
local plan = _G.__ROCK_ARTIFACT_BINDING_PLAN
assert(plan ~= nil, "compiler did not publish an Artifact binding plan")
assert(plan.census.examined > 0, "binding-plan census has no examined population")
assert(plan.census.examined == plan.census.assigned, "binding-plan census is incomplete")
local rows = {{}}
for i = 1, plan.entries.len do
  local entry = plan.entries._data[i]
  rows[#rows + 1] = table.concat({{
    entry.identity,
    entry.physical,
    entry.diagnostic,
    entry.kind,
    tostring(entry.state_key),
  }}, "=")
end
table.sort(rows)
print(table.concat(rows, "|"))
"#,
            lua = lua
        ),
        label,
    )
}

#[test]
fn compiler_plan_tracks_added_removed_and_arbitrary_top_level_bindings() {
    let before = plan_snapshot(&emit_mir(
        r#"
global ordinary: int = 1
global __rock_user_binding: int = 2
global __stone_user_binding: int = 3
global __ROCK_user_binding: int = 4
global stone_user_binding: int = 5
global replay_user_binding: int = 6
global runtime_binding_not_in_old_classifier: int = 7
fn main() {}
"#,
    ).lua, "artifact_binding_before");
    let after = plan_snapshot(&emit_mir(
        r#"
global ordinary: int = 1
global __rock_user_binding: int = 2
global __stone_user_binding: int = 3
global __ROCK_user_binding: int = 4
global stone_user_binding: int = 5
global replay_user_binding: int = 6
global runtime_binding_added: int = 8
fn main() {}
"#,
    ).lua, "artifact_binding_after");

    for name in [
        "ordinary",
        "__rock_user_binding",
        "__stone_user_binding",
        "__ROCK_user_binding",
        "stone_user_binding",
        "replay_user_binding",
        "runtime_binding_not_in_old_classifier",
    ] {
        assert!(before.contains(name), "missing compiler-plan member `{name}`: {before}");
    }
    assert!(after.contains("runtime_binding_added"), "added binding is absent: {after}");
    assert!(!after.contains("runtime_binding_not_in_old_classifier"), "removed binding survived: {after}");
}

#[test]
fn logical_identity_survives_a_backend_name_change() {
    let old = plan_snapshot(&emit_mir(
        r#"
fn stable_binding(value: int) -> int { return value }
fn main() {}
"#,
    ).lua, "artifact_binding_identity_old");
    let new = plan_snapshot(&emit_mir(
        r#"
fn stable_binding(value: int) -> float { return 1.0 }
fn main() {}
"#,
    ).lua, "artifact_binding_identity_new");
    let old_identity = old
        .split('|')
        .find(|row| row.contains("stable_binding"))
        .expect("old stable binding plan row");
    let new_identity = new
        .split('|')
        .find(|row| row.contains("stable_binding"))
        .expect("new stable binding plan row");
    assert_eq!(old_identity.split('=').next(), new_identity.split('=').next(), "logical identity changed with the backend realization: old={old} new={new}");
    assert_ne!(old_identity, new_identity, "fixture did not change a physical binding realization");
}

#[test]
fn logical_identity_is_independent_of_physical_and_diagnostic_spelling() {
    let old = ArtifactBindingPlan::from_entries(vec![ArtifactBinding {
        identity: "logical:stable".to_string(),
        physical: "old_backend_symbol".to_string(),
        diagnostic: "old_source_name".to_string(),
        kind: ArtifactBindingKind::Callable,
        state_key: None,
    }])
    .expect("old compiler plan");
    let new = ArtifactBindingPlan::from_entries(vec![ArtifactBinding {
        identity: "logical:stable".to_string(),
        physical: "new_backend_symbol".to_string(),
        diagnostic: "new_source_name".to_string(),
        kind: ArtifactBindingKind::Callable,
        state_key: None,
    }])
    .expect("new compiler plan");

    assert!(old.census.examined > 0);
    assert!(new.census.examined > 0);
    assert_eq!(old.entries[0].identity, new.entries[0].identity);
    assert_ne!(old.entries[0].physical, new.entries[0].physical);
    assert_ne!(old.entries[0].diagnostic, new.entries[0].diagnostic);
}

#[test]
fn ordinary_and_candidate_publication_have_exact_nonzero_membership() {
    let emit = emit_mir(
        r#"
global ordinary: int = 1
global added: int = 2
fn main() {}
"#,
    );
    let out = run_luajit(
        &format!(
            r#"
{lua}
local function snapshot(plan)
  local rows = {{}}
  for i = 1, plan.entries.len do
    local entry = plan.entries._data[i]
    rows[#rows + 1] = entry.identity .. "|" .. entry.physical .. "|" .. entry.kind .. "|" .. tostring(entry.state_key)
  end
  table.sort(rows)
  return table.concat(rows, ";")
end
local ordinary = snapshot(_G.__ROCK_ARTIFACT_BINDING_PLAN)
assert(ordinary ~= "", "ordinary publication has no binding members")
local candidate = {{ entries = _G.__ROCK_ARTIFACT_BINDING_PLAN.entries, census = _G.__ROCK_ARTIFACT_BINDING_PLAN.census }}
__rock_state_transaction_begin()
__rock_artifact_binding_plan_stage(candidate)
__rock_state_transaction_commit()
local shadow_publication = snapshot(_G.__ROCK_ARTIFACT_BINDING_PLAN)
assert(shadow_publication ~= "", "candidate publication has no binding members")
assert(ordinary == shadow_publication, "ordinary and candidate membership differ: " .. ordinary .. " vs " .. shadow_publication)
print(ordinary)
"#,
            lua = emit.lua
        ),
        "artifact_binding_publication",
    );
    assert!(!out.is_empty(), "publication equality produced an empty result");
}
