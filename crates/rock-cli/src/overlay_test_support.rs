use super::compile_cmd;

pub fn emit_overlay_checked(
    ws: &bone_semantic::inference::Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    reinferred: &[(
        bone_semantic::prelude::ModuleId,
        bone_semantic::hir::BodyOwner,
    )],
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
                panic!(
                    "{ctx}: expected a failure, got clean output:\n{}",
                    normalize_stdout(&o)
                )
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
        if bytes[i] == b'0'
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'x'
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
    if full_lua.contains("function clear_all_systems") {
        ""
    } else {
        SYS_STUB
    }
}

/// `main` is unmangled (no-mangle exception), so a bare `main()` entry runs
/// it on both sides without computing a mangled name. Append AFTER the
/// overlay so the overlay's fresh `main` wins over the stale baseline's.
pub fn run_parts(parts: &[&str]) -> ExecOutcome {
    exec_lua(&parts.join("\n"))
}

pub fn print_route_profile(label: &str, profile: &crate::skull_backend::SkullRouteProfile) {
    eprintln!(
        "{label}: total={:.3} project_call={:.3} route_call={:.3} snapshot={:.3} live_update={:.3} live_unaccounted={:.3} route_unaccounted={:.3} source_feed={:.3} session={:.3} diagnostics={:.3} lint={:.3} local_lint={:.3} global_lint={:.3} global_findings={:.3} suppression={:.3} suppression_calls={} report_prepare={:.3} summary_sort={:.3} render={:.3} finding={:.3} shape={:.3} checkpoint={:.3} transactions={} commit={:.3} affected={:.3} emit={:.3} write={:.3} rollback={:.3} unaccounted={:.3}",
        profile.total_ns as f64 / 1_000_000.0,
        profile.live_project_call_ns as f64 / 1_000_000.0,
        profile.route_emit_call_ns as f64 / 1_000_000.0,
        profile.session_snapshot_ns as f64 / 1_000_000.0,
        profile.live_update_ns as f64 / 1_000_000.0,
        profile.live_project_unaccounted_ns as f64 / 1_000_000.0,
        profile.route_unaccounted_ns as f64 / 1_000_000.0,
        profile.source_lookup_feed_ns as f64 / 1_000_000.0,
        profile.session_update_ns as f64 / 1_000_000.0,
        profile.diagnostics_ns as f64 / 1_000_000.0,
        profile.lint_total_ns as f64 / 1_000_000.0,
        profile.local_lint_derivation_ns as f64 / 1_000_000.0,
        profile.global_lint_dependency_ns as f64 / 1_000_000.0,
        profile.global_lint_derivation_ns as f64 / 1_000_000.0,
        profile.suppression_filtering_ns as f64 / 1_000_000.0,
        profile.suppression_filter_calls,
        profile.report_prepare_ns as f64 / 1_000_000.0,
        profile.summary_sort_ns as f64 / 1_000_000.0,
        profile.render_ns as f64 / 1_000_000.0,
        profile.finding_aggregation_ns as f64 / 1_000_000.0,
        profile.publication_shape_checks_ns as f64 / 1_000_000.0,
        profile.transaction_checkpoint_ns as f64 / 1_000_000.0,
        profile.body_transactions_started,
        profile.transaction_commit_ns as f64 / 1_000_000.0,
        profile.affected_chunk_selection_ns as f64 / 1_000_000.0,
        profile.emit_overlay_ns as f64 / 1_000_000.0,
        profile.atomic_output_write_ns as f64 / 1_000_000.0,
        profile.rollback_ns as f64 / 1_000_000.0,
        profile.unaccounted_ns as f64 / 1_000_000.0,
    );
}

pub fn direct_overlay(root: &str, target_suffix: &str, edited: &str) -> String {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(root));
    let sources = crate::unified_compile::discover_sources(&spec).expect("discover");
    let files = crate::bone_backend::session_files(&sources);
    let target = files
        .iter()
        .find(|f| f.path.ends_with(target_suffix))
        .map(|f| f.path.clone())
        .expect("main source");
    let mut session =
        crate::bone_backend::bone_session_for_emission(files, std::path::Path::new(root))
            .expect("session");
    let renderer = skull::mir_emit::RendererTarget::Gl;
    let mid = skull::user_module_id(session.workspace()).expect("user module");
    let (_, mut cache) =
        skull::mir_emit::emit_full_in_for_renderer(session.workspace(), mid, Some(root), renderer)
            .expect("fresh emit");
    let update = session.update_file(&target, edited).expect("edit");
    assert!(matches!(
        update.tier,
        bone_incremental::session::UpdateTier::BodyOnly
    ));
    let edits = update
        .body_update
        .edit
        .iter()
        .map(|edit| skull::instance_graph::SpanEdit {
            file: edit.file,
            at: edit.at,
            byte_delta: edit.byte_delta,
            line_delta: edit.line_delta,
        })
        .collect::<Vec<_>>();
    skull::mir_emit::emit_overlay_with_span_edits(
        session.workspace(),
        &mut cache,
        &update.reinferred,
        &edits,
    )
    .expect("overlay")
    .lua
}
