//! Sim CLI argument definitions, defaults and decoding.
use crate::{Args, registry};
use clap::{Arg, ArgAction};

pub(crate) fn arguments(command: clap::Command) -> clap::Command {
    registry::extend_arguments(crate::frame_commands::arguments(command), vec![
        registry::ArgumentGroup { before: "profile", arguments: vec![
            Arg::new("allocation-census").long("allocation-census")
                .value_name("PATH")
                .help("For `rock sim`: opt into a sim-wide gross Lua allocation census and write a complete JSON artifact to PATH (discovery evidence only)."),
            Arg::new("allocation-sites").long("allocation-sites")
                .value_name("DIR")
                .help("For `rock sim`: instrument compiler-known allocation sites in isolated sims and write frame-aware joined reports to DIR."),
        ] },
        registry::ArgumentGroup { before: "no-lints", arguments: vec![
            Arg::new("update-snapshots").long("update-snapshots").action(ArgAction::SetTrue)
                .help("For `rock sim`: rewrite snapshot baselines instead of diffing."),
            Arg::new("jobs").long("jobs").short('j').value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: run up to N sim children concurrently (default: CPU count, bounded by total RAM at 3.6 GiB per lane and free GPU memory less a 6 GiB reserve at 2 GiB per lane; requests above the RAM budget are rejected, while requests above only the free-GPU budget run at that budget with a warning; at most the core count; a named run narrows N to its name count first; 1 = serial). Applies to the whole-suite and multi-name forms."),
            Arg::new("full-asset-jobs").long("full-asset-jobs").value_name("R")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: allow at most R of the concurrent children to hold full-size assets (STONE_ASSET_STUB=0 with STONE_IMAGE_MAX_SIDE=0, 2-4 GB of GPU memory each); the remaining slots up to --jobs run the other entries. Default 2. Bounded by --jobs."),
        ] },
        registry::ArgumentGroup { before: "no-lints", arguments: vec![
            Arg::new("list").long("list").action(ArgAction::SetTrue)
                .help("For `rock sim`: list every `@sim` wire name in the project, one per line, then exit."),
        ] },
    ])
        .mut_arg("perf-format", |arg| arg.help("Output format for `rock perf`: \"pretty\" (default) or \"json\"."))
}

pub(crate) fn decode(m: &clap::ArgMatches, args: &mut Args) {
    crate::frame_commands::decode(m, args);
    args.allocation_census = m.get_one::<String>("allocation-census").cloned();
    args.allocation_sites = m.get_one::<String>("allocation-sites").cloned();
    args.update_snapshots = m.get_flag("update-snapshots");
    args.jobs = m.get_one::<usize>("jobs").copied();
    args.full_asset_jobs = m.get_one::<usize>("full-asset-jobs").copied();
    args.list = m.get_flag("list");
}
