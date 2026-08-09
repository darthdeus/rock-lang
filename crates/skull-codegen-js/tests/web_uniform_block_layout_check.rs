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

/// Run `stone_shader_check_uniform_block_layout` out of the real
/// `JS_WEB_RUNTIME` against a stub GL, and return what it answered.
///
/// `indices` is what `getUniformIndices` reports per member (`None` = inactive),
/// `driver` what `getActiveUniforms` reports for the offsets, and `plan` the
/// offsets the compiler computed.
struct Answer {
    ok: bool,
    /// The indices `getActiveUniforms` was actually handed.
    asked: Vec<i64>,
}

fn check(indices: &[Option<i32>], driver: &[i32], plan: &[i32]) -> Option<bool> {
    check_with(indices, driver, plan, false, false).map(|a| a.ok)
}

/// The full driver: `*_query_fails` make the corresponding WebGL call return
/// `null`, which is what a real context does when the query generates an error.
fn check_with(
    indices: &[Option<i32>],
    driver: &[i32],
    plan: &[i32],
    index_query_fails: bool,
    offset_query_fails: bool,
) -> Option<Answer> {
    if crate::common::node().arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
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
var asked = null;
var gl = {{
  INVALID_INDEX: INVALID,
  UNIFORM_BLOCK_DATA_SIZE: 1,
  UNIFORM_OFFSET: 2,
  getUniformBlockIndex: function () {{ return 7; }},
  getActiveUniformBlockParameter: function () {{ return 16; }},
  getUniformIndices: function () {{ return {js_index_result}; }},
  getActiveUniforms: function (prog, indices) {{
    // Recorded so the test can assert an INACTIVE index was never handed over:
    // WebGL 2 fails the whole query on a bad index and returns null.
    asked = Array.prototype.slice.call(indices);
    return {js_offset_result};
  }},
}};
var __SW = {{ gl: gl, shaders: {{ 1: {{ prog: {{}} }} }} }};
{func}
var answer = stone_shader_check_uniform_block_layout(
  1, "Blk", {blob:?}, 0, [{js_plan}], {count}, 4096);
console.log(JSON.stringify({{ answer: answer, logs: logs, asked: asked }}));
"#,
        count = indices.len(),
        js_index_result = if index_query_fails { "null".to_string() } else { format!("[{js_indices}]") },
        js_offset_result = if offset_query_fails { "null".to_string() } else { format!("[{js_driver}]") },
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

    let out = crate::common::node().arg(&path).output().expect("spawn node");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "node failed:\n{stdout}\n{stderr}");
    let asked = stdout
        .split("\"asked\":")
        .nth(1)
        .and_then(|t| t.split(']').next())
        .map(|t| {
            t.trim_start_matches('[')
                .split(',')
                .filter_map(|n| n.trim().parse::<i64>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(Answer { ok: stdout.contains("\"answer\":true"), asked })
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
    // The driver reply is indexed by position in the ACTIVE list, not in
    // `members`: two entries here for members 0 and 2, with member 1 skipped.
    // Reading it positionally against `members` is the defect this pairs with --
    // it would compare member 2's offset against member 1's plan entry.
    let Some(pass) = check(&[Some(0), None, Some(2)], &[0, 512], &[0, 256, 512]) else { return };
    assert!(pass, "the active members agree, so a partially-optimized block passes");

    let Some(fail) = check(&[Some(0), None, Some(2)], &[0, 999], &[0, 256, 512]) else { return };
    assert!(!fail, "an ACTIVE member that disagrees still rejects when a sibling was skipped");
}

/// An INACTIVE index must never reach `getActiveUniforms`.
///
/// WebGL 2 fails the WHOLE query on a bad index and returns null for every
/// entry, so passing the unfiltered list means one optimized-out member takes
/// down the answer for every member beside it. The native sibling filters first
/// and maps the reply back by position; this asserts the web tier does too.
#[test]
fn only_active_indices_are_queried_for_offsets() {
    let Some(a) = check_with(&[Some(4), None, Some(6)], &[0, 512], &[0, 256, 512], false, false)
    else {
        return;
    };
    assert_eq!(
        a.asked,
        vec![4, 6],
        "the inactive member must be filtered out before the offset query, not passed as INVALID_INDEX"
    );
    assert!(a.ok, "and the surviving members must still be compared against THEIR own plan offsets");
}

/// A failed query is a rejection, not a pass. Both WebGL calls here return null
/// on error, and `null[i]` is `undefined` -- which compares unequal to every
/// offset, so the disagreement path would report a bogus mismatch, while a
/// zero-length reply would sail through as a vacuous true.
#[test]
fn a_null_query_result_is_rejected() {
    let Some(a) = check_with(&[Some(0), Some(1)], &[0, 256], &[0, 256], true, false) else {
        return;
    };
    assert!(!a.ok, "a null getUniformIndices result must reject");

    let Some(b) = check_with(&[Some(0), Some(1)], &[0, 256], &[0, 256], false, true) else {
        return;
    };
    assert!(!b.ok, "a null getActiveUniforms result must reject");
}
