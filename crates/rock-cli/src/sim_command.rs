//! Dispatch the sim command.
use crate::run_sim::{
    do_list_sims, do_run_allocation_census, do_run_allocation_sites, do_run_sims, resolve_jobs,
};
use anyhow::Result;

type AllocationCommand = fn(&str, &[String], &str, Option<usize>) -> Result<()>;

/// `rock sim`: list, allocation diagnostics, or a run. Only skull serves sims.
pub(crate) fn handle_sim(args: &crate::Args) -> Result<()> {
    let diagnostic: Option<(_, _, _, AllocationCommand)> = match (
        args.allocation_census.as_deref(),
        args.allocation_sites.as_deref(),
    ) {
        (Some(_), Some(_)) => {
            anyhow::bail!("--allocation-census and --allocation-sites are separate diagnostic arms")
        }
        (None, Some(path)) => Some((
            "--allocation-sites",
            "allocation sites need a project: rock sim --allocation-sites DIR <project> [name]",
            path,
            do_run_allocation_sites,
        )),
        (Some(path), None) => Some((
            "--allocation-census",
            "allocation census needs a project: rock sim --allocation-census PATH <project> [name]",
            path,
            do_run_allocation_census,
        )),
        (None, None) => None,
    };
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
    if let Some((flag, usage, path, run)) = diagnostic {
        anyhow::ensure!(!args.list, "{flag} cannot be combined with sim --list");
        let (project, names) = files.ok_or_else(|| anyhow::anyhow!("{usage}"))?;
        run(project, names, path, args.jobs)
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
