//! The web tier's uniform-block layout check must REJECT a block whose members
//! it could not examine.
//!
//! `stone_shader_check_uniform_block_layout` skips any member the driver reports
//! as inactive -- optimized out, no index -- which makes it a deliberately
//! PARTIAL oracle. The trap is what happens when EVERY member is skipped: the
//! comparison loop runs zero times and the function returns its initial `true`,
//! reporting success for a block it never looked at. A gate arm with no
//! population asserts nothing.
//!
//! It is load-bearing rather than theoretical because of how an ARRAY block is
//! queried. GL enumerates a block's array member under its zeroth element, so
//! the host asks for `name[0]`; if that spelling ever regresses to the bare
//! name, every member of an array block misses AT ONCE. That is precisely the
//! block where "some members were skipped" stops being partial coverage.
//!
//! The native sibling (`stone-rs/src/uniform_block.rs`) rejects on zero active
//! members. The two must agree, or the web tier silently accepts what the
//! desktop one refuses -- and per CLAUDE.md the asymmetry runs the wrong way
//! here: a missing value throws on luajit and becomes `undefined` on JS, so the
//! web tier is the side more likely to fail open.
//!
//! The shipped runtime source is executed, not a copy of it.

use std::io::Write as _;
use std::process::Command;

/// Run `stone_shader_check_uniform_block_layout` out of the real
/// `JS_WEB_RUNTIME` against a stub GL, and return what it answered.
///
/// `indices` is what `getUniformIndices` reports per member (`None` = inactive),
/// `driver` what `getActiveUniforms` reports for the offsets, and `plan` the
/// offsets the compiler computed.
fn check(indices: &[Option<i32>], driver: &[i32], plan: &[i32]) -> Option<bool> {
    check_with(indices, driver, plan, false)
}

/// `no_layout` sets the stub's `__rockNoUniformLayout` self-declaration.
fn check_with(
    indices: &[Option<i32>],
    driver: &[i32],
    plan: &[i32],
    no_layout: bool,
) -> Option<bool> {
    if Command::new("node").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP web_uniform_block_layout_check: node not on PATH");
        return None;
    }

    let names: Vec<String> = (0..indices.len()).map(|i| format!("m{i}")).collect();
    let blob = format!("{}\0", names.join("\0"));
    let js_indices = indices
        .iter()
        .map(|i| i.map(|n| n.to_string()).unwrap_or_else(|| "INVALID".to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let js_driver =
        driver.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    let js_plan = plan.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");

    // Only the function under test is lifted out of the runtime. The rest of
    // JS_WEB_RUNTIME reaches for browser globals that do not exist under node,
    // and loading it would test the harness rather than the check.
    let src = skull_codegen_js::JS_WEB_RUNTIME;
    let start = src
        .find("function stone_shader_check_uniform_block_layout(")
        .expect("the runtime must define the layout check");
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("function end") + 3;
    let func = &rest[..end];

    let driver_js = format!(
        r#"
var INVALID = 0xFFFFFFFF;
var logs = [];
function __sw_log_once(m) {{ logs.push(m); }}
var gl = {{
  __rockNoUniformLayout: {no_layout},
  INVALID_INDEX: INVALID,
  UNIFORM_BLOCK_DATA_SIZE: 1,
  UNIFORM_OFFSET: 2,
  getUniformBlockIndex: function () {{ return 7; }},
  getActiveUniformBlockParameter: function () {{ return 16; }},
  getUniformIndices: function () {{ return [{js_indices}]; }},
  getActiveUniforms: function () {{ return [{js_driver}]; }},
}};
var __SW = {{ gl: gl, shaders: {{ 1: {{ prog: {{}} }} }} }};
{func}
var answer = stone_shader_check_uniform_block_layout(
  1, "Blk", {blob:?}, 0, [{js_plan}], {count}, 4096);
console.log(JSON.stringify({{ answer: answer, logs: logs }}));
"#,
        count = indices.len(),
        no_layout = no_layout
    );

    // Unique per CALL: `cargo test` runs these on threads of one process, so a
    // dir keyed on the pid alone has one test deleting another's driver file.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rock_ubl_check_{}_{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let path = dir.join("check.js");
    let mut f = std::fs::File::create(&path).expect("write driver");
    f.write_all(driver_js.as_bytes()).expect("write driver");

    let out = Command::new("node").arg(&path).output().expect("spawn node");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "node failed:\n{stdout}\n{stderr}");
    Some(stdout.contains("\"answer\":true"))
}

/// The defect. Every member inactive -> zero comparisons -> must NOT pass.
#[test]
fn a_block_with_no_active_members_is_rejected() {
    let Some(answer) = check(&[None, None, None], &[0, 0, 0], &[0, 256, 512]) else { return };
    assert!(
        !answer,
        "zero examined members must REJECT: the loop compared nothing, so a `true` here is a \
         gate arm with no population -- and for an array block, whose members are all queried \
         as `name[0]`, a spelling regression makes every member miss at once"
    );
}

/// The other side of admission. A check that rejected everything would pass the
/// test above for the wrong reason, so the agreeing case must be accepted.
#[test]
fn a_block_whose_active_members_agree_is_accepted() {
    let Some(answer) = check(&[Some(0), Some(1), Some(2)], &[0, 256, 512], &[0, 256, 512]) else {
        return;
    };
    assert!(answer, "matching offsets over a non-empty population must pass");
}

/// The check still DOES its job: a real disagreement rejects.
#[test]
fn a_block_whose_active_member_disagrees_is_rejected() {
    let Some(answer) = check(&[Some(0), Some(1), Some(2)], &[0, 999, 512], &[0, 256, 512]) else {
        return;
    };
    assert!(!answer, "a driver offset that differs from the plan must reject");
}

/// PARTIAL coverage is still coverage: some members inactive is not the zero
/// case, and the ones that ARE active are compared.
#[test]
fn a_partially_active_block_is_still_checked() {
    let Some(pass) = check(&[Some(0), None, Some(2)], &[0, 0, 512], &[0, 256, 512]) else { return };
    assert!(pass, "the active members agree, so a partially-optimized block passes");

    let Some(fail) = check(&[Some(0), None, Some(2)], &[0, 0, 999], &[0, 256, 512]) else { return };
    assert!(!fail, "an ACTIVE member that disagrees still rejects when a sibling was skipped");
}

/// The escape hatch is for a context that DECLARES it models no layout, and
/// only for one. Without the declaration the same zero-population answer is a
/// rejection -- otherwise the hatch would be the vacuous pass wearing a name.
#[test]
fn only_a_self_declared_non_oracle_may_skip_verification() {
    let Some(declared) = check_with(&[None, None, None], &[0, 0, 0], &[0, 256, 512], true) else {
        return;
    };
    assert!(declared, "a context that declares it models no std140 layout is not a failure");

    let Some(undeclared) = check_with(&[None, None, None], &[0, 0, 0], &[0, 256, 512], false) else {
        return;
    };
    assert!(
        !undeclared,
        "the SAME zero-population answer from a context that did NOT declare itself must still \
         reject -- a real driver never carries the flag"
    );
}
