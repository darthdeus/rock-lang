use bone_core::prelude::Instant;
use bone_query::BoneWorkspace;
use lsp_types::Diagnostic;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

type Diagnostics = HashMap<String, Vec<Diagnostic>>;

struct Pending {
    anchor: String,
    files: Vec<String>,
    hints: bool,
    ready: Instant,
}

#[cfg(not(target_family = "wasm"))]
struct Running {
    root: PathBuf,
    obsolete: bool,
    task: std::thread::JoinHandle<Completed>,
}

pub(crate) struct Completed {
    pub(crate) files: Vec<String>,
    pub(crate) diagnostics: Diagnostics,
}

#[derive(Default)]
pub(crate) struct AllocationWork {
    queued: BTreeMap<PathBuf, Pending>,
    #[cfg(not(target_family = "wasm"))]
    running: Option<Running>,
}

impl AllocationWork {
    pub(crate) fn pending(&self) -> bool {
        !self.queued.is_empty() || self.is_running()
    }

    fn is_running(&self) -> bool {
        #[cfg(not(target_family = "wasm"))]
        return self.running.is_some();
        #[cfg(target_family = "wasm")]
        false
    }

    pub(crate) fn schedule(
        &mut self,
        bone: &BoneWorkspace,
        anchor: &str,
        files: Vec<String>,
        hints: bool,
    ) {
        let root = bone.project_root(anchor);
        #[cfg(not(target_family = "wasm"))]
        if let Some(running) = &mut self.running
            && running.root == root
        {
            running.obsolete = true;
        }
        if files.is_empty() || !hints && !bone.has_noalloc_contract(anchor, &files) {
            self.queued.remove(&root);
        } else {
            self.queued.insert(
                root,
                Pending {
                    anchor: anchor.to_string(),
                    files,
                    hints,
                    ready: Instant::now()
                        + Duration::from_millis(if cfg!(target_family = "wasm") { 0 } else { 150 }),
                },
            );
        }
    }

    pub(crate) fn poll(&mut self, bone: &BoneWorkspace) -> Option<Completed> {
        #[cfg(not(target_family = "wasm"))]
        if let Some(running) = &self.running {
            if !running.task.is_finished() {
                return None;
            }
            let running = self.running.take().expect("finished allocation job");
            match running.task.join() {
                Ok(result) if !running.obsolete => return Some(result),
                Ok(_) => {}
                Err(_) => log::error!("Lua allocation diagnostics worker panicked"),
            }
        }

        let now = Instant::now();
        let root = self
            .queued
            .iter()
            .find(|(_, job)| job.ready <= now)?
            .0
            .clone();
        let job = self.queued.remove(&root).expect("ready allocation job");
        let snapshot =
            bone.allocation_snapshot(&job.anchor, job.files.iter().map(String::as_str), job.hints)?;
        let analyze = move || Completed {
            files: job.files,
            diagnostics: snapshot.diagnostics(),
        };
        #[cfg(not(target_family = "wasm"))]
        {
            self.running = Some(Running {
                root,
                obsolete: false,
                task: std::thread::spawn(analyze),
            });
            None
        }
        #[cfg(target_family = "wasm")]
        Some(analyze())
    }
}
