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
        registry::ArgumentGroup { before: "type-validation", arguments: vec![
            Arg::new("top").long("top").default_value("20")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim-report`: rank this many slow observations (default 20)."),
            Arg::new("sim-log-dir").long("sim-log-dir")
                .help("For `rock sim-report`: the persistent sim history root to read (default: $ROCK_SIM_LOG_DIR, else .sim-runs). Read-only."),
            Arg::new("sim-entry").long("sim-entry")
                .help("For `rock sim-report`: compare one named sim's entry envelope across every retained run instead of reporting one run."),
        ] },
        registry::ArgumentGroup { before: "no-timeout", arguments: vec![
            Arg::new("update-snapshots").long("update-snapshots").action(ArgAction::SetTrue)
                .help("For `rock sim`: rewrite snapshot baselines instead of diffing."),
            Arg::new("jobs").long("jobs").short('j').value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: run up to N sim children concurrently (default: CPU count capped by ROCK_SIM_JOBS when it is a positive integer; explicit -j overrides that environment cap; bounded by total RAM at 3.6 GiB per lane and free GPU memory less a 6 GiB reserve at 2 GiB per lane; explicit N above the RAM budget requires --jobs-force; above only the free-GPU budget it runs at that budget with a warning; at most the core count; a named run narrows N to its name count first; 1 = serial). Applies to the whole-suite and multi-name forms."),
            Arg::new("jobs-force").long("jobs-force").action(ArgAction::SetTrue)
                .requires("jobs")
                .help("For `rock sim`: run --jobs as asked above the RAM and free-GPU budgets (still bounded by CPU count)."),
            Arg::new("full-asset-jobs").long("full-asset-jobs").value_name("R")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: allow at most R of the concurrent children to hold full-size assets (STONE_ASSET_STUB=0 with STONE_IMAGE_MAX_SIDE=0, 2-4 GB of GPU memory each); the remaining slots up to --jobs run the other entries. Default 2. Bounded by --jobs."),
        ] },
        registry::ArgumentGroup { before: "watch", arguments: vec![
            Arg::new("plan").long("plan").action(ArgAction::SetTrue)
                .conflicts_with_all(["list", "allocation-census", "allocation-sites", "update-snapshots"])
                .help("For `rock sim`: compile/discover and print the selected entries and resolved job count as JSON, without spawning sims. Honors named selections and --jobs."),
            Arg::new("list").long("list").action(ArgAction::SetTrue)
                .help("For `rock sim`: list every `@sim` wire name in the project, one per line, then exit."),
        ] },
    ])
        .mut_arg("perf-format", |arg| arg.help("Output format for `rock perf`, `rock sim-report`, or `rock modules audit`: \"pretty\" (default) or \"json\"."))
        .mut_arg("lib", |arg| arg.help("Load a project as a library context when running a Rock file. Usage: rock run --lib game/ test.rock"))
}

pub(crate) fn decode(m: &clap::ArgMatches, args: &mut Args) {
    crate::frame_commands::decode(m, args);
    args.allocation_census = m.get_one::<String>("allocation-census").cloned();
    args.allocation_sites = m.get_one::<String>("allocation-sites").cloned();
    args.top = *m.get_one::<usize>("top").unwrap();
    args.sim_log_dir = m.get_one::<String>("sim-log-dir").cloned();
    args.sim_entry = m.get_one::<String>("sim-entry").cloned();
    args.update_snapshots = m.get_flag("update-snapshots");
    args.jobs = m.get_one::<usize>("jobs").copied();
    args.jobs_force = m.get_flag("jobs-force");
    args.full_asset_jobs = m.get_one::<usize>("full-asset-jobs").copied();
    args.list = m.get_flag("list");
    args.sim_plan = m.get_flag("plan");
}
