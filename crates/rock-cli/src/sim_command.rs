//! Dispatch the sim command.
use anyhow::Result;

/// `rock sim` on the active backend (list / run-all / run-one). Single source
/// for both the rock-default and skull dispatch arms; the backend selects the
/// compile + `@sim` collection (rock: `unified_compile` + `sim_registry`;
/// skull: the bone workspace + skull's emitted registry).
pub(crate) fn handle_sim(cx: &crate::CommandContext) -> Result<()> {
    let backend = cx.args.backend;
    if cx.args.allocation_census.is_some() && cx.args.allocation_sites.is_some() {
        anyhow::bail!("--allocation-census and --allocation-sites are separate diagnostic arms");
    }
    // Validate before compiling, including paths with only one sim and no pool.
    if cx.args.jobs.is_some_and(|j| j > 1) {
        let updates = cx.args.update_snapshots
            && !cx.args.sim_plan
            && cx.args.allocation_census.is_none()
            && cx.args.allocation_sites.is_none();
        // A named request (run or plan) has one lane per name; the other
        // arms learn theirs only after compiling.
        let named_request = cx.args.files.len() > 1
            && !cx.args.list
            && cx.args.allocation_census.is_none()
            && cx.args.allocation_sites.is_none();
        let lanes = named_request.then(|| cx.args.files.len() - 1);
        crate::run_sim::resolve_jobs(cx.args.jobs, lanes, updates, cx.args.jobs_force)?;
    }
    if cx.args.sim_plan {
        if cx.args.files.is_empty() {
            anyhow::bail!("sim --plan needs a project: rock sim --plan <project> [names]");
        }
        return crate::run_sim::do_plan_sims(
            backend,
            &cx.args.files[0],
            &cx.args.files[1..],
            cx.args.jobs,
            cx.args.jobs_force,
        );
    }
    if let Some(path) = cx.args.allocation_sites.as_deref() {
        if cx.args.list {
            anyhow::bail!("--allocation-sites cannot be combined with sim --list");
        }
        if cx.args.files.is_empty() {
            anyhow::bail!(
                "allocation sites need a project: rock sim --allocation-sites DIR <project> [name]"
            );
        }
        crate::run_sim::do_run_allocation_sites(
            backend,
            &cx.args.files[0],
            &cx.args.files[1..],
            path,
            cx.args.jobs,
            cx.args.jobs_force,
        )
    } else if let Some(path) = cx.args.allocation_census.as_deref() {
        if cx.args.list {
            anyhow::bail!("--allocation-census cannot be combined with sim --list");
        }
        if cx.args.files.is_empty() {
            anyhow::bail!(
                "allocation census needs a project: rock sim --allocation-census PATH <project> [name]"
            );
        }
        crate::run_sim::do_run_allocation_census(
            backend,
            &cx.args.files[0],
            &cx.args.files[1..],
            path,
            cx.args.jobs,
            cx.args.jobs_force,
        )
    } else if cx.args.list {
        if cx.args.files.is_empty() {
            anyhow::bail!("sim --list needs a project: rock sim --list <project>");
        }
        let project = &cx.args.files[0];
        crate::run_sim::do_list_sims(backend, project)
    } else if cx.args.files.is_empty() {
        anyhow::bail!("sim needs a project: rock sim <project> [name]");
    } else if cx.args.files.len() == 1 {
        // No name: run the whole suite.
        let project = &cx.args.files[0];
        crate::run_sim::do_run_all_sims(
            backend,
            project,
            cx.args.quiet,
            cx.args.update_snapshots,
            cx.args.jobs,
            cx.args.jobs_force,
            cx.args.full_asset_jobs,
        )
    } else {
        // files[0] = project, files[1..] = one or more sim names. One name
        // streams full detail; several run concise (per-sim verdict +
        // final tally). Each name may itself be a `a,b` multi-start chain.
        let project = &cx.args.files[0];
        let names = cx.args.files[1..].to_vec();
        crate::run_sim::do_run_named_sims(
            backend,
            project,
            &names,
            cx.args.quiet,
            cx.args.update_snapshots,
            cx.args.jobs,
            cx.args.jobs_force,
            cx.args.full_asset_jobs,
        )
    }
}
