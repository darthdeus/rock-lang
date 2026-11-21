//! The lint cache must publish exactly what a fresh load of the same texts publishes.

use crate::fixture::Project;
use bone_query::BoneWorkspace;

const A0: &str = "\
fn main() {
    let unused_main = 1
    print(alpha(1))
}

fn alpha(x: int) -> int {
    return beta(x)
}

fn dead_a() -> int {
    return 3
}
";

const B0: &str = "\
global SEED: int = seed()

fn seed() -> int {
    return 4
}

fn beta(x: int) -> int {
    let spare = 2
    return x + 1
}

fn dead_b(y: int) -> int {
    return y
}
";

const C0: &str = "\
fn lonely(z: int) -> int {
    let idle = z
    return 0
}
";

type Rows = Vec<(u32, u32, u32, u32, String)>;

fn lint_rows(bone: &BoneWorkspace, path: &str) -> Rows {
    let mut rows: Rows = bone
        .lint_diagnostics(path)
        .into_iter()
        .map(|d| {
            (
                d.range.start.line,
                d.range.start.character,
                d.range.end.line,
                d.range.end.character,
                d.message,
            )
        })
        .collect();
    rows.sort();
    rows
}

struct Script {
    _project: Project,
    paths: Vec<String>,
    texts: Vec<String>,
    bone: BoneWorkspace,
}

impl Script {
    fn new() -> Script {
        let files = [("a.rock", A0), ("b.rock", B0), ("c.rock", C0)];
        let manifest = ("stick.toml", "[package]\nname = \"lints\"\n");
        let project = Project::new(&[manifest, files[0], files[1], files[2]]);
        let paths: Vec<String> = files.iter().map(|(name, _)| project.path(name)).collect();
        let texts: Vec<String> = files.iter().map(|(_, text)| text.to_string()).collect();
        let mut bone = BoneWorkspace::new();
        for (path, text) in paths.iter().zip(&texts) {
            bone.update(path, text.clone());
        }
        Script {
            _project: project,
            paths,
            texts,
            bone,
        }
    }

    fn edit(&mut self, file: usize, from: &str, to: &str, step: &str) {
        let text = self.texts[file].replacen(from, to, 1);
        assert_ne!(text, self.texts[file], "step `{step}` must change the text");
        self.texts[file] = text.clone();
        std::fs::write(&self.paths[file], &text).unwrap();
        self.bone.update(&self.paths[file], text);
        self.assert_matches_fresh(step);
    }

    fn assert_matches_fresh(&self, step: &str) {
        let mut fresh = BoneWorkspace::new();
        for (path, text) in self.paths.iter().zip(&self.texts) {
            fresh.update(path, text.clone());
        }
        for path in &self.paths {
            assert_eq!(
                lint_rows(&self.bone, path),
                lint_rows(&fresh, path),
                "after `{step}`, incremental lint findings in {path} differ from a fresh load"
            );
        }
    }

    fn all_rows(&self) -> Vec<String> {
        self.paths
            .iter()
            .flat_map(|path| lint_rows(&self.bone, path))
            .map(|row| row.4)
            .collect()
    }
}
