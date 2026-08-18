//! DIAGNOSTIC: replay the function inliner over a whole workspace and report what
//! gets inlined and how much code each splice copies. Drives `rock
//! skull-inline-stats` — the lens for "is the emit blow-up a few large callees,
//! one small callee duplicated into many callers, or deep transitive
//! accumulation?". Pure analysis: it lowers + inlines like emit does but throws
//! the result away, collecting per-splice records instead of Lua.

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use rustc_hash::FxHashMap;
use skull_mir::mir::FuncRef;
use std::fmt::Write as _;

fn concrete_roots(ws: &Workspace) -> Vec<FuncRef> {
    ws.modules
        .iter()
        .filter(|module| module.id != ModuleId::builtin())
        .flat_map(|module| {
            module.functions.iter().filter_map(|(fid, f)| {
                (crate::mir_emit::is_cached_body(f) && !f.is_coroutine).then_some(FuncRef {
                    module: module.id,
                    func: fid,
                })
            })
        })
        .collect()
}

/// Headline numbers for the console; `text` is the full report written to a file.
pub struct InlineStatsReport {
    pub text: String,
    pub funcs_scanned: usize,
    pub callers_inlined: usize,
    pub total_splices: usize,
    pub total_copies: usize,
    pub distinct_callees: usize,
    pub forced: usize,
    pub over_cap: usize,
    /// `(name, body_insts, times, total_copies)` of the most-copied callee.
    pub top_callee: Option<(String, usize, usize, usize)>,
}

struct CalleeAgg {
    name: String,
    body_insts: usize,
    kind: &'static str,
    forced: bool,
    times: usize,
    total_copies: usize,
}

/// Collect inline stats over concrete body plans and format the report.
/// `callee_insts` is the reachable inst count of the (cleaned) callee body — the
/// MIR copied at a site; `total copies = callee_insts x times`, a proxy for
/// emitted code attributable to that callee.
pub fn report(ws: &Workspace) -> InlineStatsReport {
    let scx = skull_mir::lower::ScalarCtx::new(ws);
    let graph = skull_graph::instance_graph::build_in(
        ws,
        concrete_roots(ws),
        false,
        scx.interner_handle(),
    );
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let mut cache = skull_transform::InlineCache::default();

    let mut per_callee: FxHashMap<FuncRef, CalleeAgg> = FxHashMap::default();
    let mut callers: Vec<(String, usize, usize)> = Vec::new(); // (name, splices, copies)
    let (mut total_splices, mut total_copies, mut funcs_scanned) = (0usize, 0usize, 0usize);

    for (inst, plan) in concrete.plans.iter() {
        let f = &ws.module(inst.func.module).functions[inst.func.func];
        if f.is_extern || f.inline_body.is_some() || f.is_coroutine {
            continue;
        }
        funcs_scanned += 1;
        let self_ref = inst.func;
        let Ok(mut mf) = skull_mir::lower::lower_instance_with_plans(
            ws,
            &scx,
            plan,
            &concrete.plans,
        ) else {
            continue;
        };
        let Ok((_deps, splices, _scan_stats)) = skull_transform::inline_functions_with_plans(
            ws,
            &scx,
            self_ref,
            &mut mf,
            &mut cache,
            &concrete.plans,
        ) else {
            continue;
        };
        if splices.is_empty() {
            continue;
        }
        let mut caller_copies = 0usize;
        for r in &splices {
            total_splices += 1;
            total_copies += r.callee_insts;
            caller_copies += r.callee_insts;
            let agg = per_callee.entry(r.callee).or_insert_with(|| CalleeAgg {
                name: ws.module(r.callee.module).functions[r.callee.func].name.to_string(),
                body_insts: r.callee_insts,
                kind: if r.straight { "straight" } else { "general" },
                forced: r.forced,
                times: 0,
                total_copies: 0,
            });
            agg.times += 1;
            agg.total_copies += r.callee_insts;
        }
        callers.push((f.name.to_string(), splices.len(), caller_copies));
    }

    let mut callee_rows: Vec<&CalleeAgg> = per_callee.values().collect();
    callee_rows.sort_by(|a, b| b.total_copies.cmp(&a.total_copies).then(a.name.cmp(&b.name)));
    callers.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let forced = per_callee.values().filter(|c| c.forced).count();
    let over_cap = per_callee.values().filter(|c| c.body_insts > 72).count();
    let top_callee = callee_rows
        .first()
        .map(|c| (c.name.clone(), c.body_insts, c.times, c.total_copies));

    let mut t = String::new();
    let _ = writeln!(t, "skull inline stats");
    let _ = writeln!(t, "  concrete bodies scanned (inlinable callers):  {funcs_scanned}");
    let _ = writeln!(t, "  callers that inlined >=1 site:          {}", callers.len());
    let _ = writeln!(t, "  total splices:                          {total_splices}");
    let _ = writeln!(t, "  distinct callees inlined:               {}", per_callee.len());
    let _ = writeln!(t, "  total inst-copies (sum callee_insts):   {total_copies}");
    let _ = writeln!(t, "  @inline-forced callees:                 {forced}");
    let _ = writeln!(t, "  callees larger than GENERAL cap (72):   {over_cap}  (only reachable via @inline)");

    let _ = writeln!(t, "\n=== TOP 25 CALLERS BY INST-COPIES ADDED ===");
    let _ = writeln!(t, "{:>8}  {:>8}  caller", "copies", "splices");
    for (name, splices, copies) in callers.iter().take(25) {
        let _ = writeln!(t, "{copies:>8}  {splices:>8}  {name}");
    }

    let _ = writeln!(t, "\n=== ALL INLINED CALLEES (by total inst-copies) ===");
    let _ = writeln!(t, "{:>8}  {:>6}  {:>5}  {:>5}  {:<8} {:<6} callee", "copies", "times", "body", "avg", "kind", "forced");
    for c in &callee_rows {
        let _ = writeln!(
            t,
            "{:>8}  {:>6}  {:>5}  {:>5}  {:<8} {:<6} {}",
            c.total_copies, c.times, c.body_insts, c.total_copies / c.times.max(1),
            c.kind, if c.forced { "yes" } else { "" }, c.name
        );
    }

    InlineStatsReport {
        text: t,
        funcs_scanned,
        callers_inlined: callers.len(),
        total_splices,
        total_copies,
        distinct_callees: per_callee.len(),
        forced,
        over_cap,
        top_callee,
    }
}
