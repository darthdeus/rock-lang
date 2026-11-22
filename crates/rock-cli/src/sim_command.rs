//! Dispatch the sim command.
use crate::run_sim::{do_list_sims, do_run_allocation_sites, do_run_sims, resolve_jobs};
use anyhow::Result;

/// `rock sim`: list, attribute allocation sites, or run. Only skull serves sims.
pub(crate) fn handle_sim(args: &crate::Args) -> Result<()> {
    let diagnostic = args.allocation_sites.as_deref();
    // Validate before compiling, including paths with only one sim and no pool.
    if args.jobs.is_some_and(|j| j > 1) {
        // A named run has one lane per name; the other arms learn theirs only
        // after compiling.
        let named_run = diagnostic.is_none() && !args.list && args.files.len() > 1;
        let lanes = named_run.then(|| args.files.len() - 1);
        resolve_jobs(
            args.jobs,
            lanes,
            diagnostic.is_none() && args.update_snapshots,
        )?;
    }
    let files = args.files.split_first();
    if let Some(path) = diagnostic {
        anyhow::ensure!(
            !args.list,
            "--allocation-sites cannot be combined with sim --list"
        );
        let (project, names) = files.ok_or_else(|| {
            anyhow::anyhow!(
                "allocation sites need a project: rock sim --allocation-sites DIR <project> [name]"
            )
        })?;
        do_run_allocation_sites(project, names, path, args.jobs)
    } else if args.list {
        let (project, _) = files.ok_or_else(|| {
            anyhow::anyhow!("sim --list needs a project: rock sim --list <project>")
        })?;
        do_list_sims(project)
    } else {
        let (project, names) = files
            .ok_or_else(|| anyhow::anyhow!("sim needs a project: rock sim <project> [name]"))?;
        do_run_sims(
            project,
            names,
            args.quiet,
            args.update_snapshots,
            args.jobs,
            args.full_asset_jobs,
        )
    }
}
