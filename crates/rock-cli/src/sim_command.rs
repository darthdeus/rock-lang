//! Dispatch the sim command.
use anyhow::Result;

/// `rock sim`: list, allocation diagnostics, or a run. Only skull serves sims.
pub(crate) fn handle_sim(cx: &crate::CommandContext) -> Result<()> {
    if cx.args.allocation_census.is_some() && cx.args.allocation_sites.is_some() {
        anyhow::bail!("--allocation-census and --allocation-sites are separate diagnostic arms");
    }
    // Validate before compiling, including paths with only one sim and no pool.
    if cx.args.jobs.is_some_and(|j| j > 1) {
        let updates = cx.args.update_snapshots
            && cx.args.allocation_census.is_none()
            && cx.args.allocation_sites.is_none();
        // A named run has one lane per name; the other arms learn theirs only
        // after compiling.
        let named_request = cx.args.files.len() > 1
            && !cx.args.list
            && cx.args.allocation_census.is_none()
            && cx.args.allocation_sites.is_none();
        let lanes = named_request.then(|| cx.args.files.len() - 1);
        crate::run_sim::resolve_jobs(cx.args.jobs, lanes, updates)?;
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
            &cx.args.files[0],
            &cx.args.files[1..],
            path,
            cx.args.jobs,
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
            &cx.args.files[0],
            &cx.args.files[1..],
            path,
            cx.args.jobs,
        )
    } else if cx.args.list {
        if cx.args.files.is_empty() {
            anyhow::bail!("sim --list needs a project: rock sim --list <project>");
        }
        let project = &cx.args.files[0];
        crate::run_sim::do_list_sims(project)
    } else if cx.args.files.is_empty() {
        anyhow::bail!("sim needs a project: rock sim <project> [name]");
    } else {
        crate::run_sim::do_run_sims(
            &cx.args.files[0],
            &cx.args.files[1..],
            cx.args.quiet,
            cx.args.update_snapshots,
            cx.args.jobs,
            cx.args.full_asset_jobs,
        )
    }
}
