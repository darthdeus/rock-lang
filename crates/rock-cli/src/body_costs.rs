//! Opt-in per-body cost records: `ROCK_BODY_COSTS=<file>` makes a project
//! `rock compile` keep the attribution record of every emitted body and write
//! one JSON line per body (`"record": "body"`), named by the source
//! definition it came from, plus one line per module and front-end pass that
//! runs module by module (`"record": "module"`). `perf/body_costs.py` turns
//! the file into a report.
//!
//! Times are nanoseconds. Stage times are elapsed time on the worker that
//! built the body (the quantity the compile budget's `unit_cpu` sums);
//! `thread_cpu_ns` is that worker's CPU time for the whole body, present for
//! the bodies the parallel fan-out built.

use bone_semantic::inference::Workspace;
use serde_json::{Value, json};
use skull::mir_emit::{BodyCost, BodySource, EmitAttribution};
use skull_mir::mir::FuncRef;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const SCHEMA: u32 = 1;

/// The requested output file; asking also turns on the front end's
/// per-module timers.
pub(crate) fn begin() -> Option<PathBuf> {
    let path = std::env::var_os("ROCK_BODY_COSTS").filter(|path| !path.is_empty())?;
    bone_core::workspace::MODULE_TIMINGS.store(true, std::sync::atomic::Ordering::Relaxed);
    Some(PathBuf::from(path))
}

pub(crate) fn write(
    path: &Path,
    ws: &Workspace,
    attribution: &EmitAttribution,
) -> anyhow::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for (mid, pass, time) in &ws.module_timings {
        let module = ws.module(*mid);
        let record = json!({
            "schema": SCHEMA,
            "record": "module",
            "file": module.file.as_str(),
            "package": module.package.map(|p| p.to_string()),
            "pass": pass,
            "elapsed_ns": ns(*time),
        });
        serde_json::to_writer(&mut out, &record)?;
        out.write_all(b"\n")?;
    }
    for body in &attribution.bodies {
        serde_json::to_writer(&mut out, &record(ws, body))?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// `<name>@<file>:<line>`, the key callee references use too.
fn function_key(ws: &Workspace, fref: FuncRef) -> String {
    let module = ws.module(fref.module);
    let f = &module.functions[fref.func];
    format!(
        "{}@{}:{}",
        f.name,
        module.file,
        f.name_span.line_range.0 + 1
    )
}

fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

pub(crate) fn record(ws: &Workspace, body: &BodyCost) -> Value {
    let mut record = json!({
        "schema": SCHEMA,
        "record": "body",
        "emitted": body.emitted,
        "reused_lowering": body.canonical_reused,
        "thread_cpu_ns": body.thread_cpu.map(ns),
        "lower_ns": ns(body.lower),
        "inline_ns": ns(body.inline),
        "cleanup_ns": ns(body.cleanup),
        "closure_ns": ns(body.closure),
        "counted_loop_ns": ns(body.counted_loop),
        "hoisted_inline_ns": ns(body.hoisted_inline),
        "validation_ns": ns(body.validation),
        "emit_ns": ns(body.emit),
        "regalloc_ns": ns(body.regalloc),
        "hir_nodes": body.hir_nodes,
        "mir_lowered": body.mir_lowered,
        "mir_inlined": body.mir_inlined,
        "mir_final": body.mir_final,
        "lua_bytes": body.lua_bytes,
        "lua_lines": body.lua_lines,
        "splices": body.splices.iter().map(|s| json!({
            "callee": function_key(ws, s.callee),
            "count": s.count,
            "instructions": s.instructions,
        })).collect::<Vec<_>>(),
    });
    let fields = record.as_object_mut().expect("record object");
    let (kind, module, name, span) = match body.source {
        BodySource::Function(fref) => {
            let module = ws.module(fref.module);
            let f = &module.functions[fref.func];
            let kind = if !body.symbol.is_empty() {
                "specialization"
            } else if f.is_coroutine {
                "coroutine"
            } else {
                "function"
            };
            let symbol = if body.symbol.is_empty() {
                skull_codegen_lua::mangle_fn(ws, f)
            } else {
                body.symbol.clone()
            };
            fields.insert("key".into(), json!(function_key(ws, fref)));
            fields.insert("symbol".into(), json!(symbol));
            fields.insert("generic".into(), json!(!f.generics.is_empty()));
            fields.insert("synthetic".into(), json!(f.is_synthetic));
            fields.insert("params".into(), json!(f.param_decls.len()));
            fields.insert(
                "annotations".into(),
                json!(
                    f.annotations
                        .iter()
                        .map(|a| a.name.name.as_str())
                        .collect::<Vec<_>>()
                ),
            );
            (kind, module, f.name, f.span)
        }
        BodySource::Global(mid, gid) => {
            let module = ws.module(mid);
            let g = &module.globals[gid];
            ("global", module, g.name, g.span)
        }
        BodySource::Decorator(mid, did) => {
            let module = ws.module(mid);
            let d = &module.decorators[did];
            ("decorator", module, d.name, d.span)
        }
        BodySource::Unknown => {
            fields.insert("kind".into(), json!("unknown"));
            return record;
        }
    };
    fields.insert("kind".into(), json!(kind));
    fields.insert("file".into(), json!(module.file.as_str()));
    fields.insert(
        "package".into(),
        json!(module.package.map(|p| p.to_string())),
    );
    fields.insert("name".into(), json!(name.as_str()));
    fields.insert("line".into(), json!(span.line_range.0 + 1));
    fields.insert("end_line".into(), json!(span.line_range.1 + 1));
    record
}
