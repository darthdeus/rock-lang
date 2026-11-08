//! The language server's lint cache must publish exactly what a fresh load of
//! the same texts publishes, after every step of a script mixing body edits,
//! signature edits, added and removed functions, and renames.

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

struct Project {
    _dir: tempfile::TempDir,
    paths: Vec<String>,
    texts: Vec<String>,
    bone: BoneWorkspace,
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"lints\"\n",
        )
        .unwrap();
        let mut paths = Vec::new();
        let mut texts = Vec::new();
        for (name, text) in [("a.rock", A0), ("b.rock", B0), ("c.rock", C0)] {
            std::fs::write(dir.path().join(name), text).unwrap();
            paths.push(bone_query::canonicalize_lenient(
                &dir.path().join(name).to_string_lossy(),
            ));
            texts.push(text.to_string());
        }
        let mut bone = BoneWorkspace::new();
        for (path, text) in paths.iter().zip(&texts) {
            bone.update(path, text.clone());
        }
        Project {
            _dir: dir,
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

#[test]
fn lint_cache_matches_a_fresh_load_after_mixed_edits() {
    let mut p = Project::new();
    p.assert_matches_fresh("initial load");
    let initial = p.all_rows();
    for expected in ["unused_main", "spare", "idle", "dead_a", "dead_b", "lonely"] {
        assert!(
            initial.iter().any(|row| row.contains(expected)),
            "the fixture must report `{expected}`: {initial:?}"
        );
    }

    p.edit(
        0,
        "    return beta(x)\n",
        "    return beta(x) + dead_a()\n",
        "body edit makes dead_a live",
    );
    p.edit(
        1,
        "fn dead_b(y: int) -> int {\n    return y\n}",
        "fn dead_b(y: float, w: int) -> int {\n    return w\n}",
        "signature edit of an unused function",
    );
    p.edit(
        1,
        "fn beta(x: int) -> int {",
        "fn beta(x: int, extra: int = 0) -> int {",
        "signature edit of a called function",
    );
    p.edit(
        2,
        "fn lonely(",
        "fn added(q: int) -> int {\n    let waste = q\n    return lonely(1)\n}\n\nfn lonely(",
        "add a function above another",
    );
    p.edit(
        0,
        "    return beta(x) + dead_a()\n",
        "    return beta(x)\n",
        "body edit makes dead_a dead again",
    );
    p.edit(
        0,
        "\nfn dead_a() -> int {\n    return 3\n}\n",
        "",
        "remove a function",
    );
    p.edit(1, "seed", "seed2", "a global calls a missing function");
    p.edit(1, "seed()", "seed2()", "rename the function to match");
    p.edit(
        1,
        "fn beta(",
        "fn gamma(",
        "rename a function its caller still names",
    );
    p.edit(0, "beta(x)", "gamma(x)", "repair the caller");
    p.edit(
        1,
        "    let spare = 2\n    return x + 1\n",
        "    return x + 2\n",
        "body edit removes an unused variable",
    );
    p.edit(
        2,
        "    let waste = q\n    return lonely(1)\n",
        "    return lonely(q)\n",
        "body edit inside the added function",
    );
    p.edit(
        2,
        "    let idle = z\n    return 0\n}",
        "    return z\n}\n\nfn trailing() -> int {\n    let tail = 1\n    return 2\n}",
        "edit a body and add a function after it",
    );
}
