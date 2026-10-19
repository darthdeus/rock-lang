use super::compile_cmd;

pub fn emit_overlay_checked(
    ws: &bone_semantic::inference::Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)],
) -> skull::mir_emit::MirEmit {
    skull::mir_emit::emit_overlay(ws, cache, reinferred).expect("emit_overlay")
}

#[cfg(any(test, feature = "test-support"))]
pub enum ExecOutcome {
    /// Clean run; holds full stdout (module load prints + test prints).
    Output(String),
    /// Runtime error, lua load failure, spawn failure, or timeout -- carrying
    /// the diagnostic. A test that pins a SPECIFIC failure has to be able to
    /// tell it from an unrelated load error or a timeout; discarding the message
    /// made every `Errored` arm accept any failure as proof.
    Errored(String),
}

#[cfg(any(test, feature = "test-support"))]
impl ExecOutcome {
    /// The failure diagnostic, or a panic naming `ctx` if the run succeeded.
    pub fn expect_errored(self, ctx: &str) -> String {
        match self {
            ExecOutcome::Errored(d) => d,
            ExecOutcome::Output(o) => {
                panic!("{ctx}: expected a failure, got clean output:\n{}", normalize_stdout(&o))
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn exec_lua(program: &str) -> ExecOutcome {
    let timeout = Some(test_utils::HANG_BACKSTOP);
    match compile_cmd::run_lua_code_with_timeout(program, timeout) {
        Ok(out) if !out.success => ExecOutcome::Errored(out.combined()),
        Ok(out) => ExecOutcome::Output(out.combined()),
        Err(e) => ExecOutcome::Errored(format!("lua runner failed: {e}")),
    }
}

/// Replace `0x<hex>` sequences with `0xADDR`: table/cdata identity prints
/// differ per-process and would false-positive every diff.
#[cfg(any(test, feature = "test-support"))]
pub fn normalize_stdout(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'0' && i + 2 < bytes.len() && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
        {
            out.push_str("0xADDR");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// First line where `lhs` and `rhs` differ, for divergence reporting.
#[cfg(any(test, feature = "test-support"))]
pub fn first_diff_line(lhs: &str, rhs: &str) -> String {
    for (i, (a, b)) in lhs.lines().zip(rhs.lines()).enumerate() {
        if a != b {
            return format!("line {}: {:?} != {:?}", i + 1, a, b);
        }
    }
    format!(
        "line count: {} != {}",
        lhs.lines().count(),
        rhs.lines().count()
    )
}

/// Standalone `@test`/synthetic programs have no engine. Stub the full-reload
/// registry reset only when a fixture emits one; body overlays refresh the
/// CodeRefs already stored in registries.
pub const SYS_STUB: &str = "function clear_all_systems() end\n";

/// The stub is omitted when a fixture defines its own reset function.
pub fn stub_for(full_lua: &str) -> &'static str {
    if full_lua.contains("function clear_all_systems") { "" } else { SYS_STUB }
}

/// `main` is unmangled (no-mangle exception), so a bare `main()` entry runs
/// it on both sides without computing a mangled name. Append AFTER the
/// overlay so the overlay's fresh `main` wins over the stale baseline's.
pub fn run_parts(parts: &[&str]) -> ExecOutcome {
    exec_lua(&parts.join("\n"))
}
