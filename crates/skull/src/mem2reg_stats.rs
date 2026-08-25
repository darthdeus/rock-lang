use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{CompilerError, ModuleId};
use skull_transform::Mem2RegCensus;
use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Scope(bool);

impl Drop for Scope {
    fn drop(&mut self) {
        ENABLED.set(self.0);
    }
}

pub(crate) fn scope(enabled: bool) -> Scope {
    Scope(ENABLED.replace(enabled))
}

pub(crate) fn enabled() -> bool {
    ENABLED.get()
}

#[derive(Clone, Debug)]
pub struct Mem2RegBodyRow {
    pub name: String,
    pub census: Mem2RegCensus,
}

#[derive(Debug)]
pub struct Mem2RegStatsReport {
    pub text: String,
    pub bodies_scanned: usize,
    pub totals: Mem2RegCensus,
    pub rows: Vec<Mem2RegBodyRow>,
    pub lifecycle_routes: Vec<Mem2RegRouteReport>,
}

#[derive(Debug)]
pub struct Mem2RegRouteReport {
    pub name: String,
    pub bodies_scanned: usize,
    pub totals: Mem2RegCensus,
    pub rows: Vec<Mem2RegBodyRow>,
}

fn add(to: &mut Mem2RegCensus, from: Mem2RegCensus) {
    to.blocks += from.blocks;
    to.cells += from.cells;
    to.loads += from.loads;
    to.stores += from.stores;
    to.loaded_cells += from.loaded_cells;
    to.direct_promotable_cells += from.direct_promotable_cells;
    to.direct_promotable_loads += from.direct_promotable_loads;
    to.parameter_promotable_cells += from.parameter_promotable_cells;
    to.parameter_promotable_loads += from.parameter_promotable_loads;
    to.store_promotable_cells += from.store_promotable_cells;
    to.store_promotable_loads += from.store_promotable_loads;
    to.cross_block_promotable_loads += from.cross_block_promotable_loads;
    to.multi_definition_cells += from.multi_definition_cells;
    to.non_dominating_single_definition_cells += from.non_dominating_single_definition_cells;
    to.other_single_definition_cells += from.other_single_definition_cells;
    to.current_cell_slots += from.current_cell_slots;
    to.guaranteed_removable_cell_slots += from.guaranteed_removable_cell_slots;
}

fn sort_rows(rows: &mut [Mem2RegBodyRow]) {
    rows.sort_by(|a, b| {
        b.census
            .cross_block_promotable_loads
            .cmp(&a.census.cross_block_promotable_loads)
            .then(
                b.census
                    .direct_promotable_loads
                    .cmp(&a.census.direct_promotable_loads),
            )
            .then(
                b.census
                    .guaranteed_removable_cell_slots
                    .cmp(&a.census.guaranteed_removable_cell_slots),
            )
            .then(a.name.cmp(&b.name))
    });
}

fn total(rows: &[Mem2RegBodyRow]) -> Mem2RegCensus {
    let mut totals = Mem2RegCensus::default();
    for row in rows {
        add(&mut totals, row.census);
    }
    totals
}

fn reachable_names(
    report: &crate::allocation_report::AllocationReport,
    roots: &[String],
) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    let mut pending = VecDeque::from_iter(roots.iter().cloned());
    while let Some(name) = pending.pop_front() {
        if !reached.insert(name.clone()) {
            continue;
        }
        let Some(body) = report.bodies.get(&name) else {
            continue;
        };
        pending.extend(body.call_edges.iter().map(|edge| edge.callee.clone()));
    }
    reached
}

fn format_report(allocation: &crate::allocation_report::AllocationReport) -> Mem2RegStatsReport {
    let mut rows = allocation
        .bodies
        .iter()
        .filter_map(|(name, body)| {
            Some(Mem2RegBodyRow {
                name: name.clone(),
                census: body.mem2reg_census?,
            })
        })
        .collect::<Vec<_>>();
    sort_rows(&mut rows);
    let totals = total(&rows);
    let mut lifecycle_routes = Vec::new();
    for route in &allocation.lifecycle_routes {
        let reached = reachable_names(allocation, &route.roots);
        let mut route_rows = rows
            .iter()
            .filter(|row| reached.contains(&row.name))
            .cloned()
            .collect::<Vec<_>>();
        sort_rows(&mut route_rows);
        lifecycle_routes.push(Mem2RegRouteReport {
            name: route.name.clone(),
            bodies_scanned: route_rows.len(),
            totals: total(&route_rows),
            rows: route_rows,
        });
    }

    let mut text = String::new();
    let _ = writeln!(text, "skull mem2reg residual census");
    let _ = writeln!(
        text,
        "  emitted non-coroutine MIR bodies:       {}",
        rows.len()
    );
    let _ = writeln!(
        text,
        "  surviving loads / stores:              {} / {}",
        totals.loads, totals.stores
    );
    let _ = writeln!(
        text,
        "  direct cells / loads:                  {} / {}",
        totals.direct_promotable_cells, totals.direct_promotable_loads
    );
    let _ = writeln!(
        text,
        "    Lua-foldable parameter cells/loads:  {} / {}",
        totals.parameter_promotable_cells, totals.parameter_promotable_loads
    );
    let _ = writeln!(
        text,
        "    residual Store cells / loads:        {} / {}",
        totals.store_promotable_cells, totals.store_promotable_loads
    );
    let _ = writeln!(
        text,
        "  cross-block direct loads:              {}",
        totals.cross_block_promotable_loads
    );
    let _ = writeln!(
        text,
        "  multi-definition loaded cells:         {}",
        totals.multi_definition_cells
    );
    let _ = writeln!(
        text,
        "  current MIR cell colors (sum by body):  {}",
        totals.current_cell_slots
    );
    let _ = writeln!(
        text,
        "  residual removable Store colors:       {}",
        totals.guaranteed_removable_cell_slots
    );
    let _ = writeln!(text, "\n=== BODIES BY CROSS-BLOCK DIRECT OPPORTUNITY ===");
    let _ = writeln!(
        text,
        "{:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}  body",
        "xloads", "ploads", "pcells", "multi", "slots", "rm-slot", "loads"
    );
    for row in &rows {
        let c = row.census;
        if c.loads == 0 {
            continue;
        }
        let _ = writeln!(
            text,
            "{:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}  {}",
            c.cross_block_promotable_loads,
            c.direct_promotable_loads,
            c.direct_promotable_cells,
            c.multi_definition_cells,
            c.current_cell_slots,
            c.guaranteed_removable_cell_slots,
            c.loads,
            row.name,
        );
    }
    for route in &lifecycle_routes {
        let _ = writeln!(text, "\n=== ROUTE {} ===", route.name);
        let _ = writeln!(
            text,
            "  reachable measured bodies:              {}",
            route.bodies_scanned
        );
        let _ = writeln!(
            text,
            "  surviving loads / stores:              {} / {}",
            route.totals.loads, route.totals.stores
        );
        let _ = writeln!(
            text,
            "  direct cells / loads:                  {} / {}",
            route.totals.direct_promotable_cells, route.totals.direct_promotable_loads
        );
        let _ = writeln!(
            text,
            "    Lua-foldable parameter cells/loads:  {} / {}",
            route.totals.parameter_promotable_cells, route.totals.parameter_promotable_loads
        );
        let _ = writeln!(
            text,
            "    residual Store cells / loads:        {} / {}",
            route.totals.store_promotable_cells, route.totals.store_promotable_loads
        );
        let _ = writeln!(
            text,
            "  cross-block direct loads:              {}",
            route.totals.cross_block_promotable_loads
        );
        let _ = writeln!(
            text,
            "  MIR colors / removable Store colors:   {} / {}",
            route.totals.current_cell_slots, route.totals.guaranteed_removable_cell_slots
        );
        for row in &route.rows {
            let c = row.census;
            if c.loads == 0 {
                continue;
            }
            let _ = writeln!(
                text,
                "{:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}  {}",
                c.cross_block_promotable_loads,
                c.direct_promotable_loads,
                c.direct_promotable_cells,
                c.multi_definition_cells,
                c.current_cell_slots,
                c.guaranteed_removable_cell_slots,
                c.loads,
                row.name,
            );
        }
    }

    Mem2RegStatsReport {
        text,
        bodies_scanned: rows.len(),
        totals,
        rows,
        lifecycle_routes,
    }
}

pub fn report(ws: &Workspace, entry_mid: ModuleId) -> Result<Mem2RegStatsReport, CompilerError> {
    report_in(ws, entry_mid, None)
}

pub fn report_in(
    ws: &Workspace,
    entry_mid: ModuleId,
    project_root: Option<&str>,
) -> Result<Mem2RegStatsReport, CompilerError> {
    let _mem2reg = scope(true);
    let _allocation = crate::allocation_report::scope(true);
    let emit = crate::mir_emit::emit_workspace_mir_in(ws, entry_mid, project_root)?;
    Ok(format_report(&emit.allocation_report))
}
