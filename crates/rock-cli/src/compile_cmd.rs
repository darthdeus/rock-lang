use crate::*;
use std::path::Path;

/// Exec luajit on a compiled `.lua` file with stdio inherited from the
/// caller. The single execution path for `rock run` (the skull project path
/// reuses it) -- replaces the old "pipe code through luajit's stdin" approach,
/// which broke interactive apps (fm) by closing their keyboard stdin.
pub(crate) fn exec_luajit_file(
    lua_file: &Path,
    timeout: Option<std::time::Duration>,
    app_args: &[String],
) -> Result<()> {
    let mut cmd = rock_host::project::build_luajit_command(lua_file, app_args);
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // The CLI goes silent here: the next output is whatever the game
    // prints once Stone's engine init runs. luajit loading +
    // JIT-warming the generated main.lua and ffi.load-ing native libs
    // can take seconds with no signal, making a slow/failed handoff
    // look like a hang. Mark the boundary so it's attributable.
    let spawn_t = std::time::Instant::now();
    log::info!(target: "RUN", "launching luajit -> {}", lua_file.display());

    let mut child = crate::child_lifetime::spawn_bound_child(&mut cmd)
        .map_err(|e| anyhow::anyhow!("failed to exec luajit: {}", e))?;
    log::info!(target: "RUN",
        "luajit pid {} spawned in {:.0}ms; waiting for game",
        child.id(),
        spawn_t.elapsed().as_secs_f64() * 1000.0
    );
    let pid = child.id();

    let status = if let Some(t) = timeout {
        use wait_timeout::ChildExt;
        match child.wait_timeout(t).map_err(|e| anyhow::anyhow!("wait_timeout failed: {}", e))? {
            Some(s) => s,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                crate::child_lifetime::release_bound_child(pid);
                anyhow::bail!("Luajit process timed out after {} seconds.", t.as_secs_f32());
            }
        }
    } else {
        child.wait().map_err(|e| anyhow::anyhow!("wait failed: {}", e))?
    };

    crate::child_lifetime::release_bound_child(pid);
    if !status.success() {
        crate::ablation_report::exit_with_census(status.code().unwrap_or(1));
    }
    Ok(())
}
