//! Adversarial shape testing: a VALUE oracle over the value/`nobox` shape space.
//!
//! `docs/adversarial-shape-testing-plan.md` is the spec. Every gap on the
//! `inline-lua` branch was found by luck; three of them were MISCOMPILES --
//! programs that compiled clean, emitted valid Lua and produced wrong answers. A
//! harness that asked "did it compile" would have found none of them, so every
//! program here computes a result whose expected value is known at GENERATION
//! time and carries it as `@test(output = ..)`.
//!
//! Three rungs:
//!
//! - the SEED corpus (`tests/shape_corpus/*.rock`), hand-written, one file per
//!   known gap plus its controls. Runs in the default nextest profile. Its gate
//!   is rediscovering the six gaps the plan's table lists.
//! - the pairwise SWEEP, generated here, `#[ignore]`d (`just shape-sweep`). Samples
//!   storage x production x consumption x nullability x representation so every
//!   PAIR of axis values appears in at least one program.
//! - the MARKED sweep, the same shapes with the repr forced to `Nobox`. The
//!   pairwise array only samples the marked tier into a fraction of its shapes,
//!   and that tier is the one that gates marking `Vec2`.
//!
//! Every case compiles in its OWN process, which keeps one shape's verdict from
//! depending on a sibling's. Emission also accumulates across every emitted
//! definition now, so a single compile reports its whole rejection set -- between
//! them the bucket counts are a ceiling rather than a floor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    // crates/rock-cli -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/shape_corpus")
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

/// The plan's four verdicts, plus two that are properties of the HARNESS rather
/// than of the language and so must never be silently folded into them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Bucket {
    /// Compiles, runs, right answer.
    Correct,
    /// Compiles, runs, WRONG answer (or faults at runtime from clean emission).
    /// The hunt.
    Miscompile,
    /// A spanned diagnostic. Often correct; each one wants a recorded judgement.
    LoudReject,
    /// Something that should reject but emits, or a diagnostic naming the wrong
    /// thing. Derived, not observed: an expectation of LOUD REJECT that came
    /// back CORRECT, or a rejection whose text does not name the shape.
    SilentReject,
    /// Compiles, runs, RIGHT answer -- reached through a materialization the
    /// shape does not need. Not a wrong answer, so not a MISCOMPILE; but the
    /// value oracle is blind to it, which is the whole reason the emitted-shape
    /// surface exists.
    Materialized,
    /// The backend cannot express the shape and said so (`SKIP(reason)` on JS).
    /// Not a verdict about the language.
    BackendSkip,
    /// Nothing ran -- zero `@test`s executed and no diagnostic. A harness bug,
    /// never a language finding.
    Inconclusive,
}

impl Bucket {
    fn label(self) -> &'static str {
        match self {
            Bucket::Correct => "CORRECT",
            Bucket::Miscompile => "MISCOMPILE",
            Bucket::LoudReject => "LOUD REJECT",
            Bucket::SilentReject => "SILENT REJECT",
            Bucket::Materialized => "MATERIALIZED",
            Bucket::BackendSkip => "BACKEND SKIP",
            Bucket::Inconclusive => "INCONCLUSIVE",
        }
    }
}

#[derive(Debug, Clone)]
struct Observation {
    bucket: Bucket,
    /// The load-bearing line: the wrong-vs-right values, or the diagnostic.
    detail: String,
}

// ---------------------------------------------------------------------------
// Surfaces -- how a program is compiled and observed
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// `rock test` on the default (skull/luajit) backend.
    Luajit,
    /// `rock test --backend js`.
    Js,
    /// `rock shader <file> <entry>` for both profiles. The oracle is the emitted
    /// GLSL, since a shader is not `@test`-executable from here.
    Glsl { entry: &'static str },
    /// `rock compile` -- emission only, nothing runs. For shapes whose runtime
    /// needs the engine (an entry registered with `rock_shader`) but whose verdict
    /// is decided at emit anyway. It can only ever say CORRECT or LOUD REJECT.
    Emit,
}

/// Which stdlib the process reads. `stdlib/*.rock` is read from disk at runtime,
/// so the marked variant is a patched COPY plus `ROCK_STDLIB_DIR` -- one binary,
/// two stdlibs, the marker the only variable (`benchmarks/nobox-vec2/README.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StdlibVariant {
    Vanilla,
    Vec2Nobox,
}

/// Built ONCE per test process, into a directory unique to that process.
///
/// Both halves are load-bearing. It used to run per spawned `rock` against one
/// FIXED path, so every marked observation delete-and-rebuilt the stdlib that other
/// in-flight compiles were reading. `OnceLock` alone does not fix it: nextest runs
/// each test in its own process, so a shared fixed path is still two processes
/// clobbering one directory. Nothing had failed yet, which is luck rather than
/// safety -- a torn read misclassifies a verdict instead of erroring, so the
/// failure mode is a WRONG BUCKET, not a crash.
fn marked_stdlib() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(build_marked_stdlib).clone()
}

fn build_marked_stdlib() -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("rock_shape_oracle_stdlib_vec2_nobox_{}", std::process::id()));
    let src = repo_root().join("stdlib");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("rock") {
            continue;
        }
        let mut text = std::fs::read_to_string(entry.path()).unwrap();
        if entry.file_name() == std::ffi::OsStr::new("vec.rock") {
            let marked = text.replace("\nvalue Vec2 {", "\nnobox value Vec2 {");
            assert_ne!(
                marked, text,
                "stdlib/vec.rock no longer declares `value Vec2 {{`"
            );
            text = marked;
        }
        std::fs::write(dir.join(entry.file_name()), text).unwrap();
    }
    dir
}

fn run_rock(args: &[&str], stdlib: StdlibVariant) -> (String, bool) {
    let mut cmd = Command::new(rock_bin());
    cmd.current_dir(repo_root())
        .args(args)
        .env("ROCK_TEST_VERBOSE", "1");
    if stdlib == StdlibVariant::Vec2Nobox {
        cmd.env("ROCK_STDLIB_DIR", marked_stdlib());
    }
    let out = cmd.output().expect("failed to spawn the rock binary");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

/// Classification reads the REPORT, not the exit status: both backends now exit
/// nonzero on a failing `@test`, but the status alone cannot tell a miscompile
/// from a loud rejection, and the buckets need that distinction.
fn classify_test_run(text: &str, ok: bool) -> Observation {
    // The COUNT is the oracle, not a catalogue of failure spellings: an assertion
    // mismatch, a Lua runtime fault and a JS exception all land in `(N failed)`,
    // and matching on their text instead let the `cast`-hijack mutation ("attempt
    // to index local 'r0' (a number value)") read as CORRECT.
    let (passed, total, failed, skipped) = test_counts(text);
    if failed > 0 || (skipped == 0 && total > 0 && passed < total) {
        let detail: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| {
                l.contains("output mismatch") || l.contains(": error:") || l.contains("FAIL ")
            })
            .collect();
        return Observation {
            bucket: Bucket::Miscompile,
            detail: if detail.is_empty() {
                format!("{failed} @test(s) failed with no reported reason")
            } else {
                detail.join("; ")
            },
        };
    }
    if let Some(reason) = first_diagnostic(text) {
        return Observation {
            bucket: Bucket::LoudReject,
            detail: reason,
        };
    }
    if skipped > 0 {
        // `--backend js` reports an unemittable test as a SKIP with a reason, and
        // that histogram is the emitter worklist -- a fact about the backend, not
        // a verdict about the shape.
        let detail = text
            .lines()
            .map(str::trim)
            .find(|l| l.contains("JS subset") || l.contains("SKIP("))
            .unwrap_or("skipped")
            .to_string();
        return Observation {
            bucket: Bucket::BackendSkip,
            detail,
        };
    }
    if !ok {
        return Observation {
            bucket: Bucket::LoudReject,
            detail: last_nonempty(text).unwrap_or_else(|| "non-zero exit, no diagnostic".into()),
        };
    }
    if total == 0 {
        return Observation {
            bucket: Bucket::Inconclusive,
            detail: format!(
                "no @tests executed: {}",
                last_nonempty(text).unwrap_or_default()
            ),
        };
    }
    Observation {
        bucket: Bucket::Correct,
        detail: format!("{total} @test(s) passed"),
    }
}

/// "skull backend: 1/3 @tests passed (2 failed) .." -> (1, 3, 2, 0). Both
/// backends print this line; the JS one adds ", N skipped" inside the parens.
fn test_counts(text: &str) -> (usize, usize, usize, usize) {
    let Some(line) = text.lines().find(|l| l.contains("@tests passed")) else {
        return (0, 0, 0, 0);
    };
    let frac = line
        .split_whitespace()
        .find(|w| w.contains('/'))
        .unwrap_or("0/0");
    let (p, t) = frac.split_once('/').unwrap_or(("0", "0"));
    let parens = line.split_once('(').map(|(_, r)| r).unwrap_or("");
    let num_before = |word: &str| -> usize {
        let mut prev = "";
        for w in parens.split_whitespace() {
            if w.starts_with(word) {
                return prev.parse().unwrap_or(0);
            }
            prev = w;
        }
        0
    };
    (
        p.parse().unwrap_or(0),
        t.parse().unwrap_or(0),
        num_before("failed"),
        num_before("skipped"),
    )
}

fn first_diagnostic(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("[E"))
        .map(|l| {
            // "[E00] Error: <message>" -- keep the message, drop the rendering.
            l.split_once("Error: ")
                .map(|(_, m)| m.to_string())
                .unwrap_or_else(|| l.to_string())
        })
}

fn last_nonempty(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .map(str::to_string)
}

/// GLSL identifiers containing a double underscore are RESERVED (GLSL spec 3.7),
/// so a scalarized local emitted as `shift__0` is illegal source, not a style
/// nit. This is the whole oracle for the GLSL surface: `rock shader` has no
/// runtime to assert a value in, and no GLSL compiler is guaranteed present.
fn reserved_identifiers(glsl: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    let bytes: Vec<char> = glsl.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_alphabetic() || bytes[i] == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                i += 1;
            }
            let word: String = bytes[start..i].iter().collect();
            if word.contains("__") {
                found.insert(word);
            }
        } else {
            i += 1;
        }
    }
    found.into_iter().collect()
}

const GLSL_PROFILES: [&str; 2] = ["330core", "300es"];

fn run_glsl(file: &Path, entry: &str, stdlib: StdlibVariant) -> Observation {
    let mut details = Vec::new();
    for profile in GLSL_PROFILES {
        let path = file.to_string_lossy().into_owned();
        let (text, ok) = run_rock(&["shader", &path, entry, "--profile", profile], stdlib);
        if !ok {
            let reason =
                first_diagnostic(&text).unwrap_or_else(|| last_nonempty(&text).unwrap_or_default());
            return Observation {
                bucket: Bucket::LoudReject,
                detail: format!("[{profile}] {reason}"),
            };
        }
        let reserved = reserved_identifiers(&text);
        if !reserved.is_empty() {
            details.push(format!(
                "[{profile}] reserved GLSL identifiers: {}",
                reserved.join(", ")
            ));
        }
    }
    if details.is_empty() {
        Observation {
            bucket: Bucket::Correct,
            detail: "both profiles emit legal GLSL".to_string(),
        }
    } else {
        Observation {
            bucket: Bucket::Miscompile,
            detail: details.join("; "),
        }
    }
}

// ---------------------------------------------------------------------------
// The emitted-shape surface
// ---------------------------------------------------------------------------

/// Every way the Lua emitter can build a whole `P` at runtime. `<name>_ct(` is the
/// FFI-clean cdata ctor, `<name>_vt(` the GC-carrying table ctor, and
/// `__rock_create_struct("P"` the plain-struct one -- so between them they name
/// every materialization of the type, whatever tier it landed in.
///
/// The value oracle cannot see any of this: a program that boxes three times and
/// answers 7 11 reads CORRECT. That blindness is the class this surface exists for
/// -- silent inefficiency, and silent repr drift, both of which change the emitted
/// shape without changing the answer.
fn materializations(lua: &str) -> Vec<String> {
    lua.lines()
        .map(str::trim)
        // The one FIXED per-struct shim: `_G.__ROCK_C_STRUCT["P"].copy`, emitted
        // for every cdata-tier value struct whether the program boxes or not.
        .filter(|l| !l.starts_with("return P_ct(v.") && !l.starts_with("return P_vt(v."))
        .filter(|l| {
            l.contains("P_ct(") || l.contains("P_vt(") || l.contains("__rock_create_struct(\"P\"")
        })
        .map(str::to_string)
        .collect()
}

/// A materialization compared straight to `nil` is statically always non-nil, so
/// both the box and the branch are dead. Reported rather than gated: it is waste,
/// not a wrong answer, and the fix is a lowering change with its own blast radius.
fn dead_nil_compares(lua: &str) -> Vec<String> {
    materializations(lua)
        .into_iter()
        .filter(|l| l.contains("== (nil)") || l.contains("~= (nil)"))
        .collect()
}

/// Compile to Lua and hand back the emitted chunk, or the diagnostic that stopped
/// it. A rejection here is not a finding -- the value sweep already classified it.
fn emitted_lua(file: &Path, stdlib: StdlibVariant) -> Result<String, String> {
    // Never beside the input: the seed rung's inputs live in the checked-in corpus
    // dir, and an emitted `.lua` next to them is tree pollution.
    let scratch = std::env::temp_dir().join("rock_shape_oracle_emit_out");
    std::fs::create_dir_all(&scratch).unwrap();
    let out = scratch.join(file.with_extension("lua").file_name().unwrap());
    let path = file.to_string_lossy().into_owned();
    let out_path = out.to_string_lossy().into_owned();
    let (text, ok) = run_rock(&["compile", &path, &out_path], stdlib);
    if !ok {
        return Err(
            first_diagnostic(&text).unwrap_or_else(|| last_nonempty(&text).unwrap_or_default())
        );
    }
    std::fs::read_to_string(&out).map_err(|e| e.to_string())
}

fn observe(file: &Path, surface: Surface, stdlib: StdlibVariant) -> Observation {
    match surface {
        Surface::Luajit => {
            let path = file.to_string_lossy().into_owned();
            let (text, ok) = run_rock(&["test", &path], stdlib);
            classify_test_run(&text, ok)
        }
        Surface::Js => {
            let path = file.to_string_lossy().into_owned();
            let (text, ok) = run_rock(&["test", "--backend", "js", &path], stdlib);
            classify_test_run(&text, ok)
        }
        Surface::Glsl { entry } => run_glsl(file, entry, stdlib),
        Surface::Emit => {
            let path = file.to_string_lossy().into_owned();
            let out = std::env::temp_dir().join("rock_shape_oracle_emit.lua");
            let (text, ok) = run_rock(
                &["compile", &path, "--output", &out.to_string_lossy()],
                stdlib,
            );
            if let Some(reason) = first_diagnostic(&text) {
                return Observation {
                    bucket: Bucket::LoudReject,
                    detail: reason,
                };
            }
            if !ok {
                return Observation {
                    bucket: Bucket::LoudReject,
                    detail: last_nonempty(&text)
                        .unwrap_or_else(|| "non-zero exit, no diagnostic".into()),
                };
            }
            Observation {
                bucket: Bucket::Correct,
                detail: "emitted".to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The seed corpus
// ---------------------------------------------------------------------------

/// What a seed case is expected to do TODAY. A live bug's expectation is
/// `Miscompile`, so the case goes RED the moment the fix lands and the flip
/// happens in the fixing commit -- the sentinel discipline of
/// `docs/test-sentinels.md`, one line per case instead of an inverted assertion
/// inside the Rock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Correct,
    /// Live bug. `flip_when` says what closes it.
    Miscompile {
        flip_when: &'static str,
    },
    /// A rejection is owed. `names` must appear in the diagnostic -- a rejection
    /// that does not name the shape is the SILENT REJECT class.
    LoudReject {
        names: &'static str,
    },
    /// A rejection that is owed to NOTHING: the program is sound and the compiler
    /// refuses it anyway. Observed as LOUD REJECT today, so it passes; `flip_when`
    /// says what makes the diagnostic go away, and the case goes red then, forcing
    /// the same-commit update. The `LoudReject`/`SilentReject` pair cannot express
    /// this -- both assume the rejection is deserved.
    SpuriousReject {
        names: &'static str,
        flip_when: &'static str,
    },
}

struct SeedCase {
    name: &'static str,
    file: &'static str,
    surface: Surface,
    stdlib: StdlibVariant,
    expect: Expect,
    /// Which of the plan's six known gaps this case covers, if any.
    gap: Option<&'static str>,
}

const SEEDS: &[SeedCase] = &[
    SeedCase {
        name: "column_read_widen",
        file: "column_read_widen.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("column read widened to `T?` (fixed, cad7fa39d)"),
    },
    SeedCase {
        name: "cast_name_hijack",
        file: "cast_name_hijack.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("`cast` name hijack (fixed, b10cf9274)"),
    },
    SeedCase {
        name: "cast_shadow_call_shapes",
        file: "cast_shadow_call_shapes.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("`cast` name hijack, deepened to a value-struct argument"),
    },
    SeedCase {
        name: "cast_shadow_fn_value_reference",
        file: "cast_shadow_fn_value_reference.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "ambiguous fn-value reference",
        },
        gap: Some("NEW: a user `cast` is callable but not nameable as a fn value"),
    },
    SeedCase {
        name: "nullable_read_aliasing",
        file: "nullable_read_aliasing.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("nullable whole-value read aliases (fixed, presence-aware copy-out)"),
    },
    SeedCase {
        // The same lowering, one backend over. Kept separate because a fix that
        // lands in the Lua emitter only would leave this red.
        name: "nullable_read_aliasing_js",
        file: "nullable_read_aliasing.rock",
        surface: Surface::Js,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("nullable whole-value read aliases (fixed, shared MIR lowering)"),
    },
    SeedCase {
        name: "value_local_field_write",
        file: "value_local_field_write.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "immutable local",
        },
        gap: Some("value-local field write loses writes (LIVE, behind the bone relaxation)"),
    },
    SeedCase {
        name: "value_place_roots",
        file: "value_place_roots.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "fn_value_off_field",
        file: "fn_value_off_field.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "survived lowering",
        },
        gap: Some("fn value called off a struct field (LIVE)"),
    },
    SeedCase {
        name: "fn_value_off_field_bound",
        file: "fn_value_off_field_bound.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "lambda_returning_nobox_at_let",
        file: "lambda_returning_nobox_at_let.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "survived lowering",
        },
        gap: Some("NEW: a lambda returning a nobox value rejects at a `let`"),
    },
    // M3's three, each reproduced minimally against a control that EMITS before
    // being named. All are LOUD, so none is urgent; the judgement each wants is
    // the plan's M5 question, recorded in `docs/nobox-nested-ctor-plan.md`.
    SeedCase {
        name: "nobox_tuple_return_projected",
        file: "nobox_tuple_return_projected.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "survived lowering",
        },
        gap: Some("M3: a nobox projected off a fn's tuple return at the call (LIVE)"),
    },
    SeedCase {
        name: "nobox_coalesce_at_ctor_field",
        file: "nobox_coalesce_at_ctor_field.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        // Was an M5 GAP and is now CORRECT: `fold_never_nil_compares` made the
        // coalesce's box eliminable, so the nobox value stops surviving lowering.
        // This is the item-2 trace confirmed by a fix -- the mechanism was never the
        // packing, it was whether SROA could forward the pack away.
        expect: Expect::Correct,
        gap: Some("M3: `??` recovering a nullable nobox at a ctor field (LIVE)"),
    },
    SeedCase {
        name: "nobox_lambda_literal_position",
        file: "nobox_lambda_literal_position.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "survived lowering",
        },
        gap: Some("M3: a nobox-returning lambda LITERAL at an argument (LIVE)"),
    },
    SeedCase {
        name: "glsl_local_unmarked",
        file: "glsl_local_nobox_repr.rock",
        surface: Surface::Glsl {
            entry: "shape_probe_frag",
        },
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "glsl_local_vec2_marked",
        file: "glsl_local_nobox_repr.rock",
        surface: Surface::Glsl {
            entry: "shape_probe_frag",
        },
        stdlib: StdlibVariant::Vec2Nobox,
        expect: Expect::Correct,
        gap: Some("GLSL locals inherit nobox repr (fixed, `is_nobox_struct` gates on `values_first_class`)"),
    },
    SeedCase {
        name: "shader_registration_fn_body_unmarked",
        file: "shader_registration_fn_value.rock",
        surface: Surface::Emit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "shader_registration_fn_body_vec2_marked",
        file: "shader_registration_fn_value.rock",
        surface: Surface::Emit,
        stdlib: StdlibVariant::Vec2Nobox,
        expect: Expect::Correct,
        gap: Some(
            "NEW: `rock_shader(<entry>)` in a FN BODY rejects under marking; the \
             global-init form does not, and the fn is never taken by value",
        ),
    },
    SeedCase {
        name: "shader_registration_global_init_unmarked",
        file: "shader_registration_global_init.rock",
        surface: Surface::Emit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        // The control that makes the spelling the discriminant. Without it the
        // fn-body rejection reads as "marking costs a lambda per registration",
        // which is what it was first reported as and is wrong.
        name: "shader_registration_global_init_vec2_marked",
        file: "shader_registration_global_init.rock",
        surface: Surface::Emit,
        stdlib: StdlibVariant::Vec2Nobox,
        expect: Expect::Correct,
        gap: None,
    },
    // M5 judged this a GAP and it had no seed case -- it was reachable only through
    // the sweep, where a rejection is a row in a histogram rather than a pinned
    // shape. Unmarked on purpose: it is shipped-language behaviour, not a marking
    // cost. Both controls are separate FILES because a compile rejection aborts the
    // whole file, so a control beside the probe never runs.
    // The controls for the three marked-tier classes below. Each is its OWN file:
    // they used to sit beside their rejecting probe, where a compile rejection
    // aborts the whole file, so they never executed and "the control emits" was a
    // comment rather than a gate. All six pass in isolation, so the M5 judgements
    // they support stand.
    SeedCase {
        name: "nobox_tuple_return_projected_control",
        file: "nobox_tuple_return_projected_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "nobox_coalesce_at_ctor_field_control",
        file: "nobox_coalesce_at_ctor_field_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "nobox_lambda_literal_position_control",
        file: "nobox_lambda_literal_position_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    // The exemption's SCOPE. `materialize` evaluates its argument, so a fn name there
    // is a real runtime fn value: exempting every ExpandKind opened a hole that turned
    // this spanned rejection into a runtime failure.
    SeedCase {
        name: "expansion_arg_exemption_scope",
        file: "expansion_arg_exemption_scope.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "cannot be taken as a fn value",
        },
        gap: None,
    },
    SeedCase {
        name: "index_place_refinement",
        file: "index_place_refinement.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::LoudReject {
            names: "on non-struct type",
        },
        gap: Some("M5 GAP: refinement does not reach an index place (name-keyed paths)"),
    },
    SeedCase {
        name: "index_place_refinement_field_control",
        file: "index_place_refinement_field_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    SeedCase {
        name: "index_place_refinement_bound_control",
        file: "index_place_refinement_bound_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: None,
    },
    // --- silent-class sentinels (docs/nobox-case-matrix.md 19-21) -----------
    // Pinned per docs/test-sentinels.md: each `.rock` states the CORRECT
    // output, and the expectation here records that we get something else
    // TODAY. The fixing commit flips these to `Expect::Correct`.
    SeedCase {
        name: "embed_safenav_leaf_projection",
        file: "embed_safenav_leaf_projection.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("H1 FIXED: safe-nav composes through `physical_field` instead of \
                   emitting a bare-key read"),
    },
    SeedCase {
        name: "embed_safenav_leaf_projection_control",
        file: "embed_safenav_leaf_projection_control.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("H1 control: scopes the defect to safe-nav, not to embed fields"),
    },
    SeedCase {
        name: "embed_safenav_nullable_field",
        file: "embed_safenav_nullable_field.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("H1 FIXED: a nullable embed field no longer yields its presence tag"),
    },
    SeedCase {
        name: "embed_match_destructure",
        file: "embed_match_destructure.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Correct,
        gap: Some("H2 FIXED: a destructure binding rebuilds from the parent's leaves"),
    },
    SeedCase {
        name: "nobox_vec_combinator_element",
        file: "nobox_vec_combinator_element.rock",
        surface: Surface::Luajit,
        stdlib: StdlibVariant::Vanilla,
        expect: Expect::Miscompile {
            flip_when: "specialized bound calls (lower.rs:4957) consume the concrete \
                        tuple return shape instead of a hardcoded 2 slots -- a \
                        specialization/tuple-ABI fix, NOT the H1/H2 field-read fix",
        },
        gap: Some("I: generic return-shape mismatch at the bound-call boundary"),
    },
];

fn reconcile(expect: Expect, obs: &Observation) -> (Bucket, Option<String>) {
    match expect {
        Expect::Correct => match obs.bucket {
            Bucket::Correct => (Bucket::Correct, None),
            b => (
                b,
                Some(format!(
                    "expected CORRECT, got {}: {}",
                    b.label(),
                    obs.detail
                )),
            ),
        },
        Expect::Miscompile { flip_when } => match obs.bucket {
            Bucket::Miscompile => (Bucket::Miscompile, None),
            Bucket::Correct => (
                Bucket::Correct,
                Some(format!(
                    "the pinned bug is FIXED -- flip this case's expectation to Correct. \
                     Trigger recorded as: {flip_when}"
                )),
            ),
            b => (
                b,
                Some(format!(
                    "expected MISCOMPILE, got {}: {}",
                    b.label(),
                    obs.detail
                )),
            ),
        },
        Expect::SpuriousReject { names, flip_when } => match obs.bucket {
            Bucket::LoudReject if obs.detail.contains(names) => (Bucket::LoudReject, None),
            Bucket::Correct => (
                Bucket::Correct,
                Some(format!(
                    "the spurious rejection is GONE -- flip this case to Correct. \
                     Trigger recorded as: {flip_when}"
                )),
            ),
            b => (
                b,
                Some(format!(
                    "expected the pinned spurious rejection (naming {names:?}), got {}: {}",
                    b.label(),
                    obs.detail
                )),
            ),
        },
        Expect::LoudReject { names } => match obs.bucket {
            Bucket::LoudReject if obs.detail.contains(names) => (Bucket::LoudReject, None),
            Bucket::LoudReject => (
                Bucket::SilentReject,
                Some(format!(
                    "the rejection does not name the shape (wanted {names:?}): {}",
                    obs.detail
                )),
            ),
            Bucket::Correct => (
                Bucket::SilentReject,
                Some(format!(
                    "a rejection is owed here and the program EMITTED instead: {}",
                    obs.detail
                )),
            ),
            b => (
                b,
                Some(format!(
                    "expected LOUD REJECT, got {}: {}",
                    b.label(),
                    obs.detail
                )),
            ),
        },
    }
}

fn bucket_table(counts: &BTreeMap<Bucket, usize>) -> String {
    let mut out = String::from("\nbucket counts:\n");
    for bucket in [
        Bucket::Correct,
        Bucket::Miscompile,
        Bucket::LoudReject,
        Bucket::SilentReject,
        Bucket::Materialized,
        Bucket::BackendSkip,
        Bucket::Inconclusive,
    ] {
        out.push_str(&format!(
            "  {:<14} {}\n",
            bucket.label(),
            counts.get(&bucket).unwrap_or(&0)
        ));
    }
    out
}

#[test]
fn the_seed_corpus_reproduces_every_known_gap() {
    let dir = corpus_dir();
    let results: Vec<(Bucket, Option<String>, &SeedCase)> = SEEDS
        .iter()
        .map(|case| {
            let obs = observe(&dir.join(case.file), case.surface, case.stdlib);
            let (bucket, complaint) = reconcile(case.expect, &obs);
            (
                bucket,
                complaint.map(|c| format!("{}: {c}", case.name)),
                case,
            )
        })
        .collect();

    let mut counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut report = String::from("\nseed corpus (one shape per process):\n");
    for (bucket, _, case) in &results {
        *counts.entry(*bucket).or_insert(0) += 1;
        report.push_str(&format!(
            "  {:<44} {:<14} {}\n",
            case.name,
            bucket.label(),
            case.gap.unwrap_or("(control)")
        ));
    }
    report.push_str(&bucket_table(&counts));
    println!("{report}");

    let complaints: Vec<String> = results.into_iter().filter_map(|(_, c, _)| c).collect();
    assert!(complaints.is_empty(), "{report}\n{}", complaints.join("\n"));
}

// ---------------------------------------------------------------------------
// The pairwise sweep
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Repr {
    /// `value P { a: int, b: int }` -- the shipped language, no marking.
    Unmarked,
    /// `nobox value P { .. }` -- the tier that gates marking.
    Nobox,
    /// A `string` leaf forces the GC-carrying (`_vt` table) representation.
    GcCarrying,
    /// Five leaves, one past `MAX_COLUMN_LEAVES` -- column storage ineligible.
    WideLeaves,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Storage {
    Local,
    Param,
    StructField,
    ValueStructField,
    VecElement,
    MapValue,
    Global,
    NativeBufferElement,
    FlatVecElement,
    ArenaElement,
    CaptureCell,
    /// A closure REBINDS the captured name and the read happens through the
    /// outer one, so a cell that is not shared loses the write and the sentinel
    /// leaks into the answer.
    CaptureCellWritten,
    /// A local of a `co fn` written before a `yield` and consumed after the
    /// resume, so the value has to survive a suspend point in the frame.
    CoFrameAcrossYield,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Production {
    LiteralCtor,
    CallResult,
    PhiIf,
    PhiMatch,
    PhiElseIfChain,
    TupleElement,
    WholeValueRead,
    Index,
    Coalesce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Consumption {
    Bind,
    DirectCallArg,
    IndirectCallArg,
    FnValueOffField,
    PlainReturn,
    NullableReturn,
    TupleReturn,
    FieldWrite,
    CompoundFieldWrite,
    Interpolation,
    Inspect,
    /// Write a leaf through the stored copy, then read the leaf back off the
    /// place the value was READ OUT OF. Needs a production with an upstream
    /// place; the copy invariant says the source must be untouched.
    CopyOnWriteProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Nullability {
    NonNull,
    /// Stored as `P?`, refined with `!= null` at the use.
    Refined,
    /// Stored as `P?`, recovered with `??` at the use.
    Coalesced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Shape {
    repr: Repr,
    storage: Storage,
    production: Production,
    consumption: Consumption,
    nullability: Nullability,
}

impl Shape {
    fn name(&self) -> String {
        format!(
            "{:?}_{:?}_{:?}_{:?}_{:?}",
            self.repr, self.storage, self.production, self.consumption, self.nullability
        )
        .to_lowercase()
    }
}

const REPRS: [Repr; 4] = [
    Repr::Unmarked,
    Repr::Nobox,
    Repr::GcCarrying,
    Repr::WideLeaves,
];
const STORAGES: [Storage; 13] = [
    Storage::Local,
    Storage::Param,
    Storage::StructField,
    Storage::ValueStructField,
    Storage::VecElement,
    Storage::MapValue,
    Storage::Global,
    Storage::NativeBufferElement,
    Storage::FlatVecElement,
    Storage::ArenaElement,
    Storage::CaptureCell,
    Storage::CaptureCellWritten,
    Storage::CoFrameAcrossYield,
];
const PRODUCTIONS: [Production; 9] = [
    Production::LiteralCtor,
    Production::CallResult,
    Production::PhiIf,
    Production::PhiMatch,
    Production::PhiElseIfChain,
    Production::TupleElement,
    Production::WholeValueRead,
    Production::Index,
    Production::Coalesce,
];
const CONSUMPTIONS: [Consumption; 12] = [
    Consumption::Bind,
    Consumption::DirectCallArg,
    Consumption::IndirectCallArg,
    Consumption::FnValueOffField,
    Consumption::PlainReturn,
    Consumption::NullableReturn,
    Consumption::TupleReturn,
    Consumption::FieldWrite,
    Consumption::CompoundFieldWrite,
    Consumption::Interpolation,
    Consumption::Inspect,
    Consumption::CopyOnWriteProbe,
];
const NULLABILITIES: [Nullability; 3] = [
    Nullability::NonNull,
    Nullability::Refined,
    Nullability::Coalesced,
];

/// Greedy all-pairs (pairwise) cover over axes of the given cardinalities: every
/// PAIR of values drawn from two different axes appears in at least one returned
/// index tuple. Deterministic -- no RNG, so a failing sweep replays exactly.
///
/// Each round walks the whole candidate lattice in odometer order (last axis
/// fastest) and keeps the first tuple covering the most uncovered pairs. Bounded
/// by the pair count, which shrinks strictly every round.
fn odometer_next(idx: &mut [usize], axes: &[usize]) -> bool {
    for a in (0..axes.len()).rev() {
        idx[a] += 1;
        if idx[a] < axes[a] {
            return true;
        }
        idx[a] = 0;
    }
    false
}

/// A greedy pairwise covering array over the tuples `renderable` accepts.
///
/// The predicate is the whole point and it must gate BOTH halves. This used to cover
/// the full index lattice with no notion of renderability, and the sweeps then
/// `continue`d past whatever `render` declined -- so a pair whose only covering tuple
/// was unrenderable silently vanished from the executed corpus, 28 of them,
/// falsifying the plan's defining premise while every rung stayed green. Restricting
/// only the candidate walk is not enough either: `wanted` would still hold
/// infeasible pairs and the greedy loop would spin to `expect`.
/// `the_executed_corpus_covers_every_feasible_pair` is the standing check.
fn pairwise_cover(axes: &[usize], renderable: impl Fn(&[usize]) -> bool) -> Vec<Vec<usize>> {
    let candidates: Vec<Vec<usize>> = {
        let mut out = Vec::new();
        let mut idx = vec![0usize; axes.len()];
        loop {
            if renderable(&idx) {
                out.push(idx.clone());
            }
            if !odometer_next(&mut idx, axes) {
                break;
            }
        }
        out
    };

    let mut wanted: BTreeSet<(usize, usize, usize, usize)> = BTreeSet::new();
    for idx in &candidates {
        for a in 0..axes.len() {
            for b in (a + 1)..axes.len() {
                wanted.insert((a, b, idx[a], idx[b]));
            }
        }
    }

    let mut chosen: Vec<Vec<usize>> = Vec::new();
    while !wanted.is_empty() {
        let mut best: Option<(&Vec<usize>, usize)> = None;
        for idx in &candidates {
            let mut gain = 0;
            for a in 0..axes.len() {
                for b in (a + 1)..axes.len() {
                    if wanted.contains(&(a, b, idx[a], idx[b])) {
                        gain += 1;
                    }
                }
            }
            if gain > best.as_ref().map(|(_, g)| *g).unwrap_or(0) {
                best = Some((idx, gain));
            }
        }
        let (idx, _) = best.expect("feasible pairs remain but no renderable tuple covers any");
        let idx = idx.clone();
        for a in 0..axes.len() {
            for b in (a + 1)..axes.len() {
                wanted.remove(&(a, b, idx[a], idx[b]));
            }
        }
        chosen.push(idx);
    }
    chosen
}

const AXES: [usize; 5] = [
    REPRS.len(),
    STORAGES.len(),
    PRODUCTIONS.len(),
    CONSUMPTIONS.len(),
    NULLABILITIES.len(),
];

fn shape_at(i: &[usize]) -> Shape {
    Shape {
        repr: REPRS[i[0]],
        storage: STORAGES[i[1]],
        production: PRODUCTIONS[i[2]],
        consumption: CONSUMPTIONS[i[3]],
        nullability: NULLABILITIES[i[4]],
    }
}

fn shape_indices(s: &Shape) -> [usize; 5] {
    [
        REPRS.iter().position(|v| *v == s.repr).unwrap(),
        STORAGES.iter().position(|v| *v == s.storage).unwrap(),
        PRODUCTIONS.iter().position(|v| *v == s.production).unwrap(),
        CONSUMPTIONS
            .iter()
            .position(|v| *v == s.consumption)
            .unwrap(),
        NULLABILITIES
            .iter()
            .position(|v| *v == s.nullability)
            .unwrap(),
    ]
}

/// The pairs that some RENDERABLE tuple can express. Not every pair can: `render`
/// declines combinations the generator has no program for, and a pair whose only
/// witnesses are all unrenderable is infeasible rather than missing.
fn feasible_pairs(
    axes: &[usize],
    build: impl Fn(&[usize]) -> Shape,
) -> BTreeSet<(usize, usize, usize, usize)> {
    let mut feasible = BTreeSet::new();
    let mut idx = vec![0usize; axes.len()];
    loop {
        if render(build(&idx)).is_some() {
            for a in 0..axes.len() {
                for b in (a + 1)..axes.len() {
                    feasible.insert((a, b, idx[a], idx[b]));
                }
            }
        }
        if !odometer_next(&mut idx, axes) {
            break;
        }
    }
    feasible
}

fn pairs_present(
    axes: &[usize],
    executed: &[Shape],
    indices: impl Fn(&Shape) -> Vec<usize>,
) -> BTreeSet<(usize, usize, usize, usize)> {
    let mut present = BTreeSet::new();
    for s in executed {
        let i = indices(s);
        for a in 0..axes.len() {
            for b in (a + 1)..axes.len() {
                present.insert((a, b, i[a], i[b]));
            }
        }
    }
    present
}

fn pairwise_shapes() -> Vec<Shape> {
    pairwise_cover(&AXES, |i| render(shape_at(i)).is_some())
        .into_iter()
        .map(|i| shape_at(&i))
        .collect()
}

/// The plan's defining premise, asserted instead of assumed: *every PAIR of axis
/// values appears in at least one program*. It was FALSE and silently so -- the
/// cover was built over the full index lattice and `render` then filtered
/// unrenderable tuples out at execution time, so any pair whose only covering tuple
/// got filtered vanished from the corpus with nothing to say so. Measured before the
/// fix: 41 pairs absent unmarked (~28 of them feasible), including
/// `nobox x flatvecelement`, `global x coalesce` and `index x plainreturn`.
///
/// Cheap enough for the default profile: `render` is pure, so this spawns no `rock`.
#[test]
fn the_executed_corpus_covers_every_feasible_pair() {
    assert_pairwise("unmarked", &AXES, shape_at, &sweep_shapes(), |s| {
        shape_indices(s).to_vec()
    });
    // The MARKED tier needs its own assertion, not the unmarked one: its cover runs
    // over four axes with `repr` pinned, so unmarked coverage says nothing about it.
    // Ten feasible marked pairs were missing while this test passed.
    assert_pairwise(
        "marked",
        &MARKED_AXES,
        marked_shape_at,
        &marked_sweep_shapes(),
        |s| marked_shape_indices(s).to_vec(),
    );
}

fn assert_pairwise(
    tier: &str,
    axes: &[usize],
    build: fn(&[usize]) -> Shape,
    shapes: &[Shape],
    indices: impl Fn(&Shape) -> Vec<usize>,
) {
    let executed: Vec<Shape> = shapes.iter().copied().filter(|s| render(*s).is_some()).collect();
    let present = pairs_present(axes, &executed, indices);
    // Axis names in the order the tier's own array declares them.
    let all_names = ["repr", "storage", "production", "consumption", "nullability"];
    let names = &all_names[all_names.len() - axes.len()..];
    let describe = |(a, b, va, vb): &(usize, usize, usize, usize)| {
        let mut i = vec![0usize; axes.len()];
        i[*a] = *va;
        i[*b] = *vb;
        let s = build(&i);
        let val = |axis: usize| -> String {
            match names[axis] {
                "repr" => format!("{:?}", s.repr),
                "storage" => format!("{:?}", s.storage),
                "production" => format!("{:?}", s.production),
                "consumption" => format!("{:?}", s.consumption),
                _ => format!("{:?}", s.nullability),
            }
        };
        format!("{}={} x {}={}", names[*a], val(*a), names[*b], val(*b))
    };
    let missing: Vec<String> = feasible_pairs(axes, build)
        .difference(&present)
        .map(describe)
        .collect();
    assert!(
        missing.is_empty(),
        "{tier} tier: {} FEASIBLE pair(s) appear in no executed shape, so the corpus \
         is not pairwise over what it can render:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The dangerous TRIPLES the plan's table names, hand-added on top of the
/// pairwise set: a pairwise array covers every pair but not necessarily the
/// three-way interaction that actually broke.
fn dangerous_triples() -> Vec<Shape> {
    let mut out = Vec::new();
    // nullable x whole-value-read x field-root -- the live aliasing miscompile.
    // The probe is `CopyOnWriteProbe`, not `FieldWrite`: reading the write back
    // through the SAME place cannot see aliasing, only reading it back off the
    // source can. Every storage that can hold the copy is worth a case, and both
    // upstream place kinds (a field read and a column read).
    for repr in [
        Repr::Unmarked,
        Repr::Nobox,
        Repr::GcCarrying,
        Repr::WideLeaves,
    ] {
        for production in [Production::WholeValueRead, Production::Index] {
            for storage in [
                Storage::StructField,
                Storage::ValueStructField,
                Storage::Local,
                Storage::VecElement,
                Storage::Global,
            ] {
                for nullability in [Nullability::NonNull, Nullability::Refined] {
                    out.push(Shape {
                        repr,
                        storage,
                        production,
                        consumption: Consumption::CopyOnWriteProbe,
                        nullability,
                    });
                }
            }
        }
    }
    // fn-value x field-call. The seed pair puts the discriminant on the CALL SHAPE
    // (`h.cb(arg)` rejects, `let f = h.cb; f(arg)` runs), which says nothing about
    // where the ARGUMENT came from -- so vary that: every repr, every nullability,
    // both cell-shaped storages, and the productions that read out of a place.
    for repr in REPRS {
        for nullability in NULLABILITIES {
            for storage in [
                Storage::Local,
                Storage::StructField,
                Storage::VecElement,
                Storage::CaptureCellWritten,
            ] {
                for production in [
                    Production::LiteralCtor,
                    Production::CallResult,
                    Production::WholeValueRead,
                    Production::Index,
                ] {
                    out.push(Shape {
                        repr,
                        storage,
                        production,
                        consumption: Consumption::FnValueOffField,
                        nullability,
                    });
                }
            }
        }
    }
    // cast x user-shadowing is not a shape in these axes -- a user `cast` decl
    // is a whole-program property, so it deepens in the seed corpus
    // (`cast_name_hijack.rock`, `cast_shadow_call_shapes.rock`) rather than as a
    // generated axis value.
    out
}

// ---------------------------------------------------------------------------
// Program synthesis
// ---------------------------------------------------------------------------

/// A generated program plus the answer, computed HERE and not read off a run.
struct Program {
    source: String,
    /// Only for the record -- the assertion lives in the `@test(output = ..)`.
    expected: String,
}

struct Leaves {
    decl: String,
    /// `a: 7, b: 11` style ctor arguments, and the observable print.
    ctor_args: fn(i32, i32) -> String,
}

fn leaves(repr: Repr) -> Leaves {
    match repr {
        Repr::Unmarked => Leaves {
            decl: "value P { a: int, b: int }".into(),
            ctor_args: |a, b| format!("a: {a}, b: {b}"),
        },
        Repr::Nobox => Leaves {
            decl: "nobox value P { a: int, b: int }".into(),
            ctor_args: |a, b| format!("a: {a}, b: {b}"),
        },
        Repr::GcCarrying => Leaves {
            decl: "value P { a: int, b: int, tag: string }".into(),
            ctor_args: |a, b| format!("a: {a}, b: {b}, tag: \"t\""),
        },
        Repr::WideLeaves => Leaves {
            decl: "value P { a: int, b: int, c: int, d: int, e: int }".into(),
            ctor_args: |a, b| format!("a: {a}, b: {b}, c: 1, d: 2, e: 3"),
        },
    }
}

/// `render` returns None for a shape that cannot be SPELLED (a nullable element
/// of a `NativeBuffer<P>`, say). Unrepresentable is a fact about the generator,
/// never a verdict about the language, so those are counted and named in the
/// report instead of being folded into a bucket.
fn render(shape: Shape) -> Option<Program> {
    let l = leaves(shape.repr);
    // Every `??` fallback in a generated program is DEAD: the place always holds a
    // present value, so the coalesce must yield the place. Giving the fallback
    // leaves the expectation does NOT have keeps a wrongly-taken fallback from
    // reading as CORRECT -- a fallback spelled `P { a: 7, .. }` would answer right
    // for the wrong reason.
    let ctor_dead = format!("P {{ {} }}", (l.ctor_args)(-1, -1));
    let mut decls: Vec<String> = vec![l.decl.clone(), "fn seed(n: int) -> int { return n }".into()];
    let mut body: Vec<String> = Vec::new();
    // Statements go in already indented: a nullable source or a refinement opens
    // a block, and the leaf write has to land INSIDE it.
    let mut depth = 1usize;
    let ind = |d: usize| "    ".repeat(d);
    // Set by the productions that read out of an upstream PLACE -- the only ones
    // that can observe whether the read copied.
    let mut source_place: Option<String> = None;
    // Whether the upstream place is still `P?` where the probe reads it.
    let mut source_nullable = false;
    let nullable = shape.nullability != Nullability::NonNull;

    // The ctor whose `a` leaf comes from a call, so nothing const-folds the
    // shape away before it is lowered.
    let seeded_ctor = format!(
        "P {{ {} }}",
        (l.ctor_args)(7, 11).replace("a: 7", "a: seed(7)")
    );

    // -- production: an expression of type P.
    let produced: String = match shape.production {
        Production::LiteralCtor => seeded_ctor.clone(),
        Production::CallResult => {
            decls.push(format!(
                "fn mk(n: int) -> P {{ return P {{ {} }} }}",
                (l.ctor_args)(7, 11).replace("a: 7", "a: n")
            ));
            "mk(seed(7))".into()
        }
        Production::PhiIf => format!(
            "if seed(1) > 0 {{ {seeded_ctor} }} else {{ P {{ {} }} }}",
            (l.ctor_args)(0, 0)
        ),
        Production::PhiMatch => format!(
            "match seed(1) {{ 1 => {seeded_ctor}, _ => P {{ {} }} }}",
            (l.ctor_args)(0, 0)
        ),
        Production::PhiElseIfChain => format!(
            "if seed(1) > 2 {{ P {{ {} }} }} else if seed(1) > 1 {{ P {{ {} }} }} \
             else if seed(1) > 0 {{ {seeded_ctor} }} else {{ P {{ {} }} }}",
            (l.ctor_args)(1, 1),
            (l.ctor_args)(2, 2),
            (l.ctor_args)(0, 0)
        ),
        Production::TupleElement => {
            decls.push(format!(
                "fn mk_pair(n: int) -> (int, P) {{ return (n, P {{ {} }}) }}",
                (l.ctor_args)(7, 11).replace("a: 7", "a: n")
            ));
            "mk_pair(seed(7)).1".into()
        }
        Production::WholeValueRead => {
            // The SOURCE field carries the nullability. That is the discriminant
            // for the live aliasing bug: a whole-value read of a `P?` field
            // declines the copy-out, a read of a `P` field does not -- so a
            // nullable DESTINATION alone reproduces nothing.
            // Read UNREFINED: the copy-out is keyed on the expression's
            // nullability at the read, so refining the source first turns the
            // read back into the non-nullable one that copies correctly. Both
            // names refine at the USE instead.
            let src_ty = if nullable { "P?" } else { "P" };
            decls.push(format!("struct Src {{ p: {src_ty} }}"));
            body.push(format!(
                "{}let src = Src {{ p: {seeded_ctor} }}",
                ind(depth)
            ));
            source_nullable = nullable;
            source_place = Some("src.p".into());
            "src.p".into()
        }
        Production::Index => {
            // A nullable element would need refinement through an INDEX place,
            // which does not refine (see KNOWN_REJECTIONS), so the column stays
            // non-null and the nullability lands on the destination.
            body.push(format!("{}let src: [P] = [{seeded_ctor}]", ind(depth)));
            source_place = Some("src[0]".into());
            "src[0]".into()
        }
        Production::Coalesce => {
            decls.push(format!(
                "fn maybe(n: int) -> P? {{ if n > 0 {{ return P {{ {} }} }} return null }}",
                (l.ctor_args)(7, 11).replace("a: 7", "a: n")
            ));
            format!("(maybe(seed(7)) ?? {ctor_dead})")
        }
    };

    // -- what the storage slot holds. A GLOBAL needs an opaque widening call:
    // bone refines a global from its own write, so `GP = P { .. }` then
    // `GP != null` / `GP ?? d` are correctly rejected as redundant. Nothing else
    // gets one -- routing the value through a call packs a fresh copy at the ABI
    // boundary and would hide the aliasing these cases exist to catch.
    let stored_ty = if nullable { "P?" } else { "P" };
    let produced = match (nullable, shape.storage) {
        // Already `P?` when the source field carried the nullability -- widening
        // it again would just be a type error.
        (true, Storage::Global) if !source_nullable => {
            decls.push("fn as_maybe(p: P) -> P? { return p }".into());
            format!("as_maybe({produced})")
        }
        _ => produced,
    };

    // -- storage: park the value and hand back a place expression.
    let place: String = match shape.storage {
        Storage::Local => {
            body.push(format!("{}let slot: {stored_ty} = {produced}", ind(depth)));
            "slot".into()
        }
        Storage::Param => {
            decls.push(format!(
                "fn thru(x: {stored_ty}) -> {stored_ty} {{ return x }}"
            ));
            body.push(format!(
                "{}let slot: {stored_ty} = thru({produced})",
                ind(depth)
            ));
            "slot".into()
        }
        Storage::StructField => {
            decls.push(format!("struct Holder {{ p: {stored_ty} }}"));
            body.push(format!("{}let h = Holder {{ p: {produced} }}", ind(depth)));
            "h.p".into()
        }
        Storage::ValueStructField => {
            if nullable && shape.repr == Repr::Nobox {
                // A nullable nobox field forces the outer to the GC tier; kept
                // out so the case is about ONE tier.
                return None;
            }
            decls.push(format!("value Outer {{ p: {stored_ty}, k: int }}"));
            body.push(format!(
                "{}let o = Outer {{ p: {produced}, k: 1 }}",
                ind(depth)
            ));
            "o.p".into()
        }
        Storage::VecElement => {
            body.push(format!(
                "{}let xs: [{stored_ty}] = [{produced}]",
                ind(depth)
            ));
            "xs[0]".into()
        }
        Storage::MapValue => {
            body.push(format!(
                "{}let m: Map<string, {stored_ty}> = map_new()",
                ind(depth)
            ));
            body.push(format!("{}m[\"k\"] = {produced}", ind(depth)));
            "m[\"k\"]".into()
        }
        Storage::Global => {
            decls.push(format!("global GP: {stored_ty}"));
            body.push(format!("{}GP = {produced}", ind(depth)));
            "GP".into()
        }
        Storage::NativeBufferElement => {
            if nullable || shape.repr == Repr::GcCarrying {
                return None; // the element type must be FFI-clean and non-null
            }
            body.push(format!(
                "{}let buf: NativeBuffer<P> = native_buffer_alloc(1)",
                ind(depth)
            ));
            body.push(format!("{}buf[0] = {produced}", ind(depth)));
            "buf[0]".into()
        }
        Storage::FlatVecElement => {
            if nullable || shape.repr == Repr::GcCarrying {
                return None;
            }
            body.push(format!(
                "{}let fv: FlatVec<P> = flat_vec_new(2)",
                ind(depth)
            ));
            body.push(format!("{}fv.push({produced})", ind(depth)));
            "fv[0]".into()
        }
        Storage::ArenaElement => {
            decls.push(format!("struct Cell {{ p: {stored_ty} }}"));
            body.push(format!(
                "{}let arena: Arena<Cell> = arena_new()",
                ind(depth)
            ));
            body.push(format!(
                "{}let ent = arena.insert(Cell {{ p: {produced} }})",
                ind(depth)
            ));
            // `Arena` indexing is generation-checked, so the element read is
            // always `T?`; unwrap it so the shape under test is the FIELD, not
            // the handle's nullability.
            body.push(format!("{}let cell = ent.get().unwrap()", ind(depth)));
            "cell.p".into()
        }
        Storage::CaptureCell => {
            // The lambda is BOUND to a local and then called, not handed to a
            // higher-order fn as a literal: `call_it(() => captured)` puts the
            // shape's weight on lambda-literal-in-argument-position, where a
            // compiler is free to inline the body at the call site and never build
            // a cell at all -- so it was not testing capture.
            body.push(format!(
                "{}let captured: {stored_ty} = {produced}",
                ind(depth)
            ));
            body.push(format!("{}let f = () => captured", ind(depth)));
            body.push(format!("{}let slot: {stored_ty} = f()", ind(depth)));
            "slot".into()
        }
        Storage::CaptureCellWritten => {
            // The SENTINEL is what makes a lost write visible: the produced value
            // arrives only through the closure, so a cell that is not shared with
            // the outer name leaves 0/null in the answer instead of 7/11.
            let sentinel = if nullable {
                "null".to_string()
            } else {
                format!("P {{ {} }}", (l.ctor_args)(0, 0))
            };
            body.push(format!(
                "{}let captured: {stored_ty} = {sentinel}",
                ind(depth)
            ));
            body.push(format!(
                "{}let w = () => {{ captured = {produced} }}",
                ind(depth)
            ));
            body.push(format!("{}w()", ind(depth)));
            "captured".into()
        }
        Storage::CoFrameAcrossYield => {
            body.push(format!("{}let slot: {stored_ty} = {produced}", ind(depth)));
            // The suspend point. Everything above it runs on the first resume,
            // everything below on the second, so `slot` has to live in the frame.
            body.push(format!("{}yield 0", ind(depth)));
            "slot".into()
        }
    };

    // -- refinement: from the stored (possibly nullable) place to a P.
    let value: String = if !nullable {
        place.clone()
    } else {
        match shape.nullability {
            Nullability::Refined => {
                // Both names refine in ONE condition, which is what lets the
                // probe read the source leaf plainly.
                let also = match (&source_place, source_nullable) {
                    (Some(src), true) => format!(" && {src} != null"),
                    _ => String::new(),
                };
                body.push(format!("{}if {place} != null{also} {{", ind(depth)));
                depth += 1;
                if !also.is_empty() {
                    source_nullable = false;
                }
                place.clone()
            }
            _ => {
                body.push(format!("{}let got: P = {place} ?? {ctor_dead}", ind(depth)));
                "got".into()
            }
        }
    };

    // -- consumption: observe, and say what the answer must be.
    let (stmts, expected): (Vec<String>, String) = match shape.consumption {
        Consumption::Bind => (
            vec![format!("let v = {value}"), "print(\"{v.a} {v.b}\")".into()],
            "7 11".into(),
        ),
        Consumption::DirectCallArg => {
            decls.push("fn show(p: P) { print(\"{p.a} {p.b}\") }".into());
            (vec![format!("show({value})")], "7 11".into())
        }
        Consumption::IndirectCallArg => {
            decls.push("fn apply(f: (P) -> int, p: P) -> int { return f(p) }".into());
            (
                vec![format!(
                    "print(\"{{apply(q => q.a, {value})}} {{{value}.b}}\")"
                )],
                "7 11".into(),
            )
        }
        Consumption::FnValueOffField => {
            decls.push("struct Handler { cb: (P) -> int }".into());
            (
                vec![
                    "let handler = Handler { cb: q => q.a }".into(),
                    format!("print(\"{{handler.cb({value})}} {{{value}.b}}\")"),
                ],
                "7 11".into(),
            )
        }
        Consumption::PlainReturn => {
            decls.push("fn ident(p: P) -> P { return p }".into());
            (
                vec![
                    format!("let v = ident({value})"),
                    "print(\"{v.a} {v.b}\")".into(),
                ],
                "7 11".into(),
            )
        }
        Consumption::NullableReturn => {
            decls.push("fn maybe_ident(p: P) -> P? { return p }".into());
            (
                vec![
                    format!("let v = maybe_ident({value})"),
                    "if v != null { print(\"{v.a} {v.b}\") }".into(),
                ],
                "7 11".into(),
            )
        }
        Consumption::TupleReturn => {
            decls.push("fn with_tag(p: P) -> (P, int) { return (p, 1) }".into());
            (
                vec![
                    format!("let t = with_tag({value})"),
                    "print(\"{t.0.a} {t.0.b}\")".into(),
                ],
                "7 11".into(),
            )
        }
        Consumption::FieldWrite => (
            vec![
                format!("{value}.a = 99"),
                format!("print(\"{{{value}.a}} {{{value}.b}}\")"),
            ],
            "99 11".into(),
        ),
        Consumption::CompoundFieldWrite => (
            vec![
                format!("{value}.a += 92"),
                format!("print(\"{{{value}.a}} {{{value}.b}}\")"),
            ],
            "99 11".into(),
        ),
        Consumption::Interpolation => (
            vec![format!("print(\"{{{value}.a}} {{{value}.b}}\")")],
            "7 11".into(),
        ),
        Consumption::CopyOnWriteProbe => {
            let src = source_place.clone()?;
            if src == value {
                // The storage IS the source (no copy happened), so there are not
                // two names to compare -- nothing to probe.
                return None;
            }
            let src_leaf = if source_nullable {
                format!("{src}?.a ?? -1")
            } else {
                format!("{src}.a")
            };
            (
                vec![
                    format!("{value}.a = 99"),
                    format!("print(\"{{{value}.a}} {{{src_leaf}}}\")"),
                ],
                "99 7".into(),
            )
        }
        Consumption::Inspect => {
            // The render itself cannot be the assertion: `@test(output = ..)` is
            // an interpolated string literal, so a `P { a: 7 }` expectation
            // parses its braces as a format specifier. Probe the leaves instead.
            //
            // Both spellings are accepted because the render LEAKS THE TIER: an
            // FFI-clean value struct renders `P { a: 7, b: 11 }` while the same
            // struct with a `string` leaf renders a multi-line `P {\n  a = 7,`
            // table dump. Reported as a lead, not fixed here; the oracle must not
            // depend on which tier the repr lands in.
            (
                vec![
                    format!("let rendered = inspect({value})"),
                    "let hit_a = rendered.contains(\"a: 7\") || rendered.contains(\"a = 7\")"
                        .into(),
                    "let hit_b = rendered.contains(\"b: 11\") || rendered.contains(\"b = 11\")"
                        .into(),
                    "print(\"{hit_a} {hit_b}\")".into(),
                ],
                "true true".into(),
            )
        }
    };
    for s in stmts {
        body.push(format!("{}{s}", ind(depth)));
    }
    while depth > 1 {
        depth -= 1;
        body.push(format!("{}}}", ind(depth)));
    }

    let mut source = String::new();
    for d in dedup(decls) {
        source.push_str(&d);
        source.push('\n');
    }
    if shape.storage == Storage::CoFrameAcrossYield {
        // The shape lives in the co fn; `shape_case` only drives it. Two resumes:
        // the first runs up to the suspend point, the second runs the consumption.
        source.push_str("\nco fn probe() -> Coroutine<int> {\n");
        for stmt in body {
            source.push_str(&format!("{stmt}\n"));
        }
        source.push_str("    yield 1\n}\n");
        source.push_str(&format!(
            "\n@test(output = \"{expected}\")\nfn shape_case() {{\n    \
             let co = probe()\n    co.resume()\n    co.resume()\n}}\n"
        ));
        return Some(Program { source, expected });
    }
    source.push_str(&format!(
        "\n@test(output = \"{expected}\")\nfn shape_case() {{\n"
    ));
    for stmt in body {
        source.push_str(&format!("{stmt}\n"));
    }
    source.push_str("}\n");
    Some(Program { source, expected })
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

/// Shapes that MISCOMPILE today, by generated case name. Every entry is a live
/// bug with a seed case pinning it; the list is what keeps the sweep a gate for
/// NEW miscompiles rather than a permanent red. Delete an entry in the commit
/// that fixes it -- an entry whose shape has started passing fails the sweep too,
/// so neither direction can drift.
const KNOWN_MISCOMPILES: &[(&str, &str)] = &[];

/// Diagnostics the sweep has SEEN and judged. An unrecognised rejection is a
/// finding OR a generator bug, and either way it must not pass quietly -- so the
/// sweep fails on one until it is classified here.
///
/// **Every entry must be specific to ONE diagnostic.** A substring broad enough to
/// match a family turns the gate off for that whole family, silently: a bare
/// `"nobox"` entry matched every marked-tier message (they all open ``nobox value
/// `P` ..``) and classified 44 shapes as already-judged. Mutation-test the GATE,
/// not only the cases -- renaming a sibling entry must make the sweep red.
///
/// **Every entry here is load-bearing, measured against both corpora** -- which
/// produce the same five diagnostic strings, so the verdict is corpus-independent.
/// Nine entries were deleted, and they were dead in two different ways:
///
/// - EIGHT name real diagnostics (each still grep-able in `crates/*/src/`) that no
///   generated shape reaches, because `render` SKIPS the shapes that would produce
///   them -- nullable / GC-carrying `NativeBuffer` and `FlatVec` elements, a
///   nullable nobox `ValueStructField`. Widening the generator is expected to bring
///   some back, LOUDLY, as UNCLASSIFIED. Two of them (`"FlatVec"`,
///   `"NativeBuffer"`) were bare one-word family prefixes -- the hazard above,
///   invisible only because nothing reached them.
/// - ONE, `"not a mutable place"`, matched no diagnostic anywhere in the tree: it
///   was written from a guess at the wording, never observed. No generator widening
///   revives it.
///
/// Deleting was the safe direction either way, and that is the asymmetry worth
/// remembering: an ABSENT entry fails loudly (UNCLASSIFIED, red sweep), an
/// OVER-BROAD one fails silently (dead gate, green tree). So for an entry nothing
/// exercises, delete unless it earns its keep.
const KNOWN_REJECTIONS: &[&str] = &[
    // Null refinement does not reach an INDEX place: `if xs[0] != null { xs[0].a }`
    // and the `Map` equivalent both reject, while the same shape through a FIELD
    // place refines. Recorded as a judgement, per the plan's M5: a queued
    // feature, not intended residue -- but a loud rejection, not a miscompile.
    "on non-struct type",
    // The SAME gap one consumption over: at a call ARGUMENT the unrefined index
    // place is reported as a type mismatch instead, so one cause has two
    // diagnostic spellings and a text-keyed reader would have judged them
    // separately.
    "type mismatch: expected `P`, found `P?`",
    // ONE entry for the value-local field write, not three. `"immutable local"`,
    // `"cannot assign"` and ``"`value` struct"`` all matched this same message, so
    // deleting any one of them left the gate green -- the milder form of the bare
    // `"nobox"` failure, and just as invisible.
    "it is an immutable local/temporary",
    "survived lowering",
    // NOT the bare substring "nobox". Every marked-tier diagnostic opens with
    // "nobox value `P` ..", so a bare "nobox" entry classifies the WHOLE tier as
    // already-judged and the unclassified gate stops gating exactly where it
    // matters most. Found by mutation: renaming the "survived lowering" entry
    // left the sweep GREEN, with its 44 shapes silently reabsorbed under
    // "nobox". An `inspect` of a nobox value IS a real boxed boundary, so the
    // specific rejection is what gets judged. Every entry names one diagnostic;
    // `known_rejections_cannot_swallow_a_diagnostic_family` enforces it.
    "cannot cross boxed call boundary",
];

/// `KNOWN_REJECTIONS` classifies by SUBSTRING, so an entry broad enough to match a
/// whole diagnostic FAMILY silently turns the unclassified-rejection gate off for
/// that family -- and it turns off green, which is the one failure mode a textual
/// merge preserves happily. Measured on this corpus: with a bare `"nobox"` present,
/// renaming the `"survived lowering"` entry left the sweep PASSING with every nobox
/// shape reabsorbed; removing it turned the same mutation red.
///
/// So the family PREFIXES are banned outright. This is a merge gate as much as a
/// design one: two branches editing this array can union it back into a dead gate,
/// and no sweep result would say so.
#[test]
fn known_rejections_cannot_swallow_a_diagnostic_family() {
    // Each of these opens an entire class of diagnostics rather than naming one.
    const FAMILY_PREFIXES: &[&str] = &["nobox", "value", "cannot", "field", "type", "error"];
    for banned in FAMILY_PREFIXES {
        assert!(
            !KNOWN_REJECTIONS.contains(banned),
            "`{banned}` matches the opening of a whole diagnostic family, so it \
             classifies every member as already-judged and the unclassified-rejection \
             gate stops gating. Record the specific boundary instead."
        );
    }
    // A entry contained in another is either redundant or the broader one is
    // swallowing the narrower one's family.
    for (i, a) in KNOWN_REJECTIONS.iter().enumerate() {
        for (j, b) in KNOWN_REJECTIONS.iter().enumerate() {
            assert!(
                i == j || !b.contains(*a),
                "`{a}` is a substring of `{b}`: one of them is dead. Keep the \
                 specific one."
            );
        }
    }
}

fn sweep_shapes() -> Vec<Shape> {
    let mut shapes = pairwise_shapes();
    shapes.extend(dangerous_triples());
    shapes.sort();
    shapes.dedup();
    shapes
}

/// M3's synthetic half: every sweep shape with its repr forced to `nobox`. The
/// pairwise array only samples `Nobox` into a fraction of the shapes, so the
/// axis appears but the marked tier's own cross-product does not -- and that
/// tier is where "zero in-tree instances" has twice meant "nobody wrote it yet".
/// Forcing the repr re-derives the whole sweep at the tier that gates marking.
/// The marked tier's own axes: `repr` is PINNED, so the cover runs over the other
/// four. Rewriting the repr of a mixed-repr cover afterwards does NOT give a pairwise
/// marked corpus -- some shapes become unrenderable at `Nobox` and the pairs only
/// they carried vanish, ten of them, with the unmarked coverage test blind to it.
const MARKED_AXES: [usize; 4] = [
    STORAGES.len(),
    PRODUCTIONS.len(),
    CONSUMPTIONS.len(),
    NULLABILITIES.len(),
];

fn marked_shape_at(i: &[usize]) -> Shape {
    Shape {
        repr: Repr::Nobox,
        storage: STORAGES[i[0]],
        production: PRODUCTIONS[i[1]],
        consumption: CONSUMPTIONS[i[2]],
        nullability: NULLABILITIES[i[3]],
    }
}

fn marked_shape_indices(s: &Shape) -> [usize; 4] {
    let i = shape_indices(s);
    [i[1], i[2], i[3], i[4]]
}

fn marked_sweep_shapes() -> Vec<Shape> {
    let mut shapes: Vec<Shape> =
        pairwise_cover(&MARKED_AXES, |i| render(marked_shape_at(i)).is_some())
            .into_iter()
            .map(|i| marked_shape_at(&i))
            .collect();
    // The dangerous triples still ride along, forced to the marked tier.
    shapes.extend(dangerous_triples().into_iter().map(|mut s| {
        s.repr = Repr::Nobox;
        s
    }));
    shapes.sort();
    shapes.dedup();
    shapes
}

/// Collapse a diagnostic to the KNOWN_REJECTIONS key it matched, so the by-shape
/// listing groups by the judged class rather than by incidental wording.
fn reason_class(detail: &str) -> String {
    KNOWN_REJECTIONS
        .iter()
        .find(|k| detail.contains(**k))
        .map(|k| (*k).to_string())
        .unwrap_or_else(|| format!("UNCLASSIFIED: {detail}"))
}

/// One sweep's worth of observations, kept as data so each rung asserts what it
/// means: the luajit rungs gate unclassified rejections, the JS one cannot
/// (an unbuilt emitter arm is not a language rule).
struct SweepRun {
    report: String,
    miscompiles: Vec<String>,
    pinned_hits: Vec<String>,
    unknown_rejections: Vec<String>,
}

fn run_sweep(label: &str, shapes: &[Shape], surface: Surface, tmp: &str) -> SweepRun {
    let dir = std::env::temp_dir().join(tmp);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut unrepresentable = 0usize;
    let mut miscompiles: Vec<String> = Vec::new();
    let mut pinned_hits: Vec<String> = Vec::new();
    let mut unknown_rejections: Vec<String> = Vec::new();
    let mut rejection_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut rejections_by_shape: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for shape in shapes {
        let Some(prog) = render(*shape) else {
            unrepresentable += 1;
            continue;
        };
        let file = dir.join(format!("{}.rock", shape.name()));
        std::fs::write(&file, &prog.source).unwrap();
        let obs = observe(&file, surface, StdlibVariant::Vanilla);
        *counts.entry(obs.bucket).or_insert(0) += 1;
        match obs.bucket {
            Bucket::Miscompile => {
                let name = shape.name();
                if KNOWN_MISCOMPILES.iter().any(|(n, _)| *n == name) {
                    pinned_hits.push(name);
                } else {
                    miscompiles.push(format!(
                        "{}\n    expected {:?}, {}\n    source:\n{}",
                        name,
                        prog.expected,
                        obs.detail,
                        indent_block(&prog.source)
                    ));
                }
            }
            Bucket::LoudReject => {
                *rejection_reasons.entry(obs.detail.clone()).or_insert(0) += 1;
                rejections_by_shape
                    .entry(reason_class(&obs.detail))
                    .or_default()
                    .push(shape.name());
                if !KNOWN_REJECTIONS.iter().any(|k| obs.detail.contains(k)) {
                    unknown_rejections.push(format!(
                        "{}: {}\n    source:\n{}",
                        shape.name(),
                        obs.detail,
                        indent_block(&prog.source)
                    ));
                }
            }
            Bucket::Inconclusive => unknown_rejections.push(format!(
                "{} produced no verdict: {}\n    source:\n{}",
                shape.name(),
                obs.detail,
                indent_block(&prog.source)
            )),
            _ => {}
        }
    }

    let mut report = format!(
        "\n{label}: {} shapes, {} unrepresentable (not a verdict)\n",
        shapes.len(),
        unrepresentable
    );
    report.push_str(&bucket_table(&counts));
    report.push_str("\nrejection reasons:\n");
    for (reason, n) in &rejection_reasons {
        report.push_str(&format!("  {n:>3}  {reason}\n"));
    }
    // Grouped BY SHAPE as well, because the reason text alone cannot be judged:
    // in the marked tier every materialization rejection reads "survived
    // lowering .. while emitting `shape_case`", so one histogram row can hide
    // several distinct shape classes. M5 wants a judgement per class, and the
    // class is a property of the shape.
    report.push_str("\nrejections by shape:\n");
    for (reason, names) in &rejections_by_shape {
        report.push_str(&format!("  {} x {reason}\n", names.len()));
        for n in names {
            report.push_str(&format!("      {n}\n"));
        }
    }
    println!("{report}");

    SweepRun {
        report,
        miscompiles,
        pinned_hits,
        unknown_rejections,
    }
}

fn assert_sweep(run: &SweepRun, gate_rejections: bool) {
    let report = &run.report;
    // A MISCOMPILE here is the whole point of the harness, so it fails loudly.
    // The live ones are pinned in KNOWN_MISCOMPILES; a NEW one must be reported,
    // not absorbed.
    assert!(
        run.miscompiles.is_empty(),
        "{report}\nMISCOMPILES ({}):\n{}",
        run.miscompiles.len(),
        run.miscompiles.join("\n")
    );
    // The other direction: a pinned shape that has started producing the right
    // answer is a FIX, and its entry has to go in the fixing commit.
    let stale: Vec<&str> = KNOWN_MISCOMPILES
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !run.pinned_hits.iter().any(|hit| hit == n))
        .collect();
    assert!(
        stale.is_empty(),
        "{report}\npinned miscompiles that no longer reproduce -- delete them from \
         KNOWN_MISCOMPILES in the commit that fixed them: {}",
        stale.join(", ")
    );
    if gate_rejections {
        assert!(
            run.unknown_rejections.is_empty(),
            "{report}\nunclassified rejections -- judge each (intended residue, or a \
             gap to build?) and add its text to KNOWN_REJECTIONS:\n{}",
            run.unknown_rejections.join("\n")
        );
    }
}

/// `#[ignore]` and not an env-var early `return`: the early return reported PASS
/// in 0.003s, indistinguishable in nextest's summary from a sweep that ran.
#[test]
#[ignore = "opt-in: spawns one rock process per shape (`just shape-sweep`)"]
fn the_pairwise_sweep_finds_no_unexpected_miscompile() {
    let run = run_sweep(
        "pairwise sweep",
        &sweep_shapes(),
        Surface::Luajit,
        "rock_shape_oracle_sweep",
    );
    assert_sweep(&run, true);
}

/// M3, the tier that GATES MARKING. Same shapes, repr forced to `nobox`, so the
/// marked tier gets the whole cross-product instead of the slice the pairwise
/// array happened to sample it into. Emission accumulates across definitions
/// now, so a rejection here is a CEILING on the shape rather than the first
/// sibling of an unknown set.
#[test]
#[ignore = "opt-in: spawns one rock process per shape (`just shape-sweep`)"]
fn the_marked_sweep_finds_no_unexpected_miscompile() {
    let run = run_sweep(
        "marked sweep (every shape forced to `nobox`)",
        &marked_sweep_shapes(),
        Surface::Luajit,
        "rock_shape_oracle_sweep_marked",
    );
    assert_sweep(&run, true);
}

/// Hand-written shapes whose property under test is the EMITTED SHAPE, not the
/// answer. `boxes` is the observed count today; the file's own header says why it
/// is what it is, so a change in either direction lands as a red test with a
/// readable reason rather than as invisible drift.
struct EmitSeed {
    name: &'static str,
    file: &'static str,
    boxes: usize,
}

const EMIT_SEEDS: &[EmitSeed] = &[
    // The floor: an unmarked value struct that never leaves the function is a
    // register set, not an object. If this ever stops being 0, scalarization has
    // regressed for the plainest shape in the language.
    EmitSeed {
        name: "scalarized_local",
        file: "emit_scalarized_local.rock",
        boxes: 0,
    },
    // The scalar ABI's whole point: a value crossing a call boundary travels as
    // leaves, so neither the caller nor the callee builds one.
    EmitSeed {
        name: "call_boundary",
        file: "emit_call_boundary.rock",
        boxes: 0,
    },
    // The GC-carrying tier through a `Map<string, P?>`. A real box IS owed here (a
    // Map value is a Lua table slot); it used to cost three, one of them fed
    // straight into a `== nil` test that could not be false. `fold_never_nil_compares`
    // removed that one. The remaining two are the store and the read's
    // re-materialization -- still one more than owed, and still a live lead.
    EmitSeed {
        name: "nullable_map_roundtrip",
        file: "emit_nullable_map_roundtrip.rock",
        boxes: 2,
    },
];

/// The emitted-shape rung. Two gates that need no per-shape hand-counting:
///
/// - sweep-wide: a `nobox` shape that EMITTED must materialize `P` zero times.
///   That is the marking guarantee itself, and a violation is a SILENT REJECT --
///   the materialization verifier owed a diagnostic and did not give one.
/// - the `EMIT_SEEDS` rung: hand-checked counts on hand-written files.
#[test]
#[ignore = "opt-in: spawns one rock process per shape (`just shape-sweep`)"]
fn the_emitted_shape_holds_the_materialization_invariants() {
    let dir = std::env::temp_dir().join("rock_shape_oracle_emit");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut complaints: Vec<String> = Vec::new();
    let mut counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut box_histogram: BTreeMap<usize, usize> = BTreeMap::new();
    let mut dead_compares: Vec<String> = Vec::new();

    for shape in &sweep_shapes() {
        let Some(prog) = render(*shape) else { continue };
        let name = shape.name();
        let file = dir.join(format!("{name}.rock"));
        std::fs::write(&file, &prog.source).unwrap();
        let lua = match emitted_lua(&file, StdlibVariant::Vanilla) {
            Ok(lua) => lua,
            Err(_) => {
                *counts.entry(Bucket::LoudReject).or_insert(0) += 1;
                continue;
            }
        };
        let boxes = materializations(&lua);
        *box_histogram.entry(boxes.len()).or_insert(0) += 1;
        if !dead_nil_compares(&lua).is_empty() {
            dead_compares.push(name.clone());
        }
        if shape.repr == Repr::Nobox && !boxes.is_empty() {
            *counts.entry(Bucket::SilentReject).or_insert(0) += 1;
            complaints.push(format!(
                "{name}: a `nobox` shape EMITTED with {} materialization(s) of P -- \
                 the guarantee was violated with no diagnostic:\n      {}",
                boxes.len(),
                boxes.join("\n      ")
            ));
            continue;
        }
        *counts
            .entry(if boxes.is_empty() {
                Bucket::Correct
            } else {
                Bucket::Materialized
            })
            .or_insert(0) += 1;
    }

    let corpus = corpus_dir();
    let mut seed_report = String::from("\nemitted-shape seeds:\n");
    for seed in EMIT_SEEDS {
        let lua = match emitted_lua(&corpus.join(seed.file), StdlibVariant::Vanilla) {
            Ok(lua) => lua,
            Err(e) => {
                complaints.push(format!("{}: expected to EMIT, rejected: {e}", seed.name));
                continue;
            }
        };
        // A pinned box count on a program that answers WRONG would be a seed that
        // proves nothing, so the value oracle still runs over these too.
        let obs = observe(
            &corpus.join(seed.file),
            Surface::Luajit,
            StdlibVariant::Vanilla,
        );
        if obs.bucket != Bucket::Correct {
            complaints.push(format!(
                "{}: the emitted-shape seed must also ANSWER right, got {}: {}",
                seed.name,
                obs.bucket.label(),
                obs.detail
            ));
        }
        let boxes = materializations(&lua);
        seed_report.push_str(&format!(
            "  {:<24} {} box(es), expected {}\n",
            seed.name,
            boxes.len(),
            seed.boxes
        ));
        if boxes.len() != seed.boxes {
            complaints.push(format!(
                "{}: {} materialization(s) of P, pinned at {} -- if this is an \
                 intended change, update the count AND the file's header:\n      {}",
                seed.name,
                boxes.len(),
                seed.boxes,
                boxes.join("\n      ")
            ));
        }
    }

    let mut report = String::from("\nemitted shape over the sweep:\n");
    report.push_str(&bucket_table(&counts));
    report.push_str("\nmaterializations per emitting shape:\n");
    for (boxes, n) in &box_histogram {
        report.push_str(&format!("  {boxes:>3} box(es)  {n} shape(s)\n"));
    }
    report.push_str(&format!(
        "\nshapes emitting a materialization fed straight into a `== nil` test \
         (dead box + dead branch): {}\n",
        dead_compares.len()
    ));
    for name in &dead_compares {
        report.push_str(&format!("  {name}\n"));
    }
    report.push_str(&seed_report);
    println!("{report}");

    assert!(complaints.is_empty(), "{report}\n{}", complaints.join("\n"));

    // A materialization fed STRAIGHT into `== nil` is dead twice over: a freshly
    // built cdata/table is never nil, so the box is wasted and the branch cannot be
    // taken. The value oracle reads every one of these as CORRECT -- the answer IS
    // right -- which is exactly the blind spot this rung exists to cover.
    assert!(
        dead_compares.is_empty(),
        "{report}\n{} shape(s) box a value only to compare it against nil. The box \
         and the branch are both dead; a fresh ctor is never nil.",
        dead_compares.len()
    );
}

// GLSL has NO generated rung, and that is a property of the target rather than a
// gap in this harness: the shapes above are user value structs over `int` leaves,
// while GLSL accepts only the tier-0 whitelist (`vecN`, `Mat4`) and turns the
// scalar ABI off entirely (`values_first_class`). There is no GLSL-legal image of
// the generated corpus to run, so the GLSL rung has its OWN generator over
// float-vector types -- `the_glsl_subset_agrees_across_backends` below.

/// The JS backend over the SAME generated shapes. It is a real rung, not a
/// placeholder -- but its verdict set differs on purpose: `--backend js` reports
/// `SKIP(reason)` for shapes its emitter does not cover yet (that histogram IS
/// the emitter worklist, `docs/js-backend-plan.md`), and a rejection there can be
/// an unbuilt emitter arm rather than a language rule. So this test gates on the
/// one thing that means the same on every backend: a program that compiles, runs
/// and answers wrong.
#[test]
#[ignore = "opt-in: spawns one rock process per shape (`just shape-sweep`)"]
fn the_pairwise_sweep_on_js_finds_no_unexpected_miscompile() {
    let shapes = sweep_shapes();
    let dir = std::env::temp_dir().join("rock_shape_oracle_sweep_js");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut miscompiles: Vec<String> = Vec::new();
    let mut pinned_hits: Vec<String> = Vec::new();
    for shape in &shapes {
        let Some(prog) = render(*shape) else { continue };
        let name = shape.name();
        let file = dir.join(format!("{name}.rock"));
        std::fs::write(&file, &prog.source).unwrap();
        let obs = observe(&file, Surface::Js, StdlibVariant::Vanilla);
        *counts.entry(obs.bucket).or_insert(0) += 1;
        if obs.bucket == Bucket::Miscompile {
            if KNOWN_MISCOMPILES.iter().any(|(n, _)| *n == name) {
                pinned_hits.push(name);
            } else {
                miscompiles.push(format!(
                    "{name}\n    expected {:?}, {}\n    source:\n{}",
                    prog.expected,
                    obs.detail,
                    indent_block(&prog.source)
                ));
            }
        }
    }
    let mut report = format!(
        "\npairwise sweep on --backend js: {} shapes\n",
        shapes.len()
    );
    report.push_str(&bucket_table(&counts));
    report.push_str(&format!(
        "  pinned (also miscompile on luajit): {}\n",
        pinned_hits.len()
    ));
    println!("{report}");
    // INCONCLUSIVE is a HARNESS bug, never a verdict about the language: zero
    // `@test`s executed and no diagnostic, which is what summary-parser drift looks
    // like. The luajit sweep already fails on it; this rung asserted only on
    // miscompiles, so a parser break turned every shape INCONCLUSIVE and the rung
    // still passed -- 339 of them, measured. `BackendSkip` stays permitted: an
    // unbuilt emitter arm IS a real fact about the JS backend.
    let inconclusive = counts.get(&Bucket::Inconclusive).copied().unwrap_or(0);
    assert_eq!(
        inconclusive, 0,
        "{report}\n{inconclusive} shape(s) produced NO VERDICT on js. That is a \
         harness fault, not a language finding -- the runner reported neither a \
         `@test` count nor a diagnostic, so this rung proved nothing about them."
    );
    assert!(
        miscompiles.is_empty(),
        "{report}\nMISCOMPILES on js that luajit does not have ({}):\n{}",
        miscompiles.len(),
        miscompiles.join("\n")
    );
}

fn indent_block(text: &str) -> String {
    text.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The GLSL-legal subset (A5)
// ---------------------------------------------------------------------------
//
// A separate shape space, because the interesting question on this rung is not
// "does the answer come out right" but "is the same shape legal on both sides".
// GLSL admits only float-vector value types, has no nullability, no strings and
// no collections; a `@shader` fn is nevertheless an ordinary Rock fn, so ONE
// generated program is observable on all three backends and the DIVERGENCE
// between their verdicts is the finding.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VecTy {
    Vec2,
    Vec3,
    Vec4,
}

impl VecTy {
    fn name(self) -> &'static str {
        match self {
            VecTy::Vec2 => "Vec2",
            VecTy::Vec3 => "Vec3",
            VecTy::Vec4 => "Vec4",
        }
    }
    fn fields(self) -> &'static [&'static str] {
        match self {
            VecTy::Vec2 => &["x", "y"],
            VecTy::Vec3 => &["x", "y", "z"],
            VecTy::Vec4 => &["x", "y", "z", "w"],
        }
    }
    /// Dyadic component values, so every derived expectation prints identically
    /// under LuaJIT's `%.14g` and JS `Number::toString`.
    fn comps(self) -> Vec<f64> {
        (0..self.fields().len())
            .map(|i| 1.0 + 0.5 * i as f64)
            .collect()
    }
    fn ctor(self, comps: &[f64]) -> String {
        let inner: Vec<String> = self
            .fields()
            .iter()
            .zip(comps)
            .map(|(f, v)| format!("{f}: {}", fmt_lit(*v)))
            .collect();
        format!("{} {{ {} }}", self.name(), inner.join(", "))
    }
    fn zero(self) -> String {
        self.ctor(&vec![0.0; self.fields().len()])
    }
    fn literal(self) -> String {
        self.ctor(&self.comps())
    }
}

/// Where the value the shape exercises comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GlslSource {
    /// Literal constants in the shader body.
    Local,
    /// A `@uniform` value-struct field -- the CPU/GPU binding boundary.
    UniformField,
    /// A helper fn's parameter, with production AND consumption inside the helper.
    HelperParam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GlslProduction {
    /// Rebuild field by field.
    LiteralCtor,
    /// Through a helper returning the vector whole.
    CallResult,
    /// Both arms of an `if` yield the vector.
    PhiIf,
    /// Vector `+` vector.
    ArithAdd,
    /// Read back out of a NON-whitelisted user value struct, RETURNED from a
    /// helper so the ctor/field-read pair cannot fold away before GLSL sees it.
    /// Inlined at the definition it folds to the bare vector and probes nothing.
    UserStructField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GlslConsumption {
    /// Rebind, then read components inline.
    Bind,
    /// Pass whole to a helper taking the vector.
    HelperArg,
    /// Return it whole from a helper, then read components.
    ReturnWhole,
    /// A tier-0 templated intrinsic (`dot`).
    Intrinsic,
    /// A `@gpu_only` intrinsic (`texture`). Legal on GLSL, and nowhere else.
    GpuOnlyIntrinsic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GlslShape {
    vec: VecTy,
    source: GlslSource,
    production: GlslProduction,
    consumption: GlslConsumption,
    stdlib: StdlibVariant,
}

impl GlslShape {
    fn name(&self) -> String {
        format!(
            "{:?}_{:?}_{:?}_{:?}_{}",
            self.vec,
            self.source,
            self.production,
            self.consumption,
            match self.stdlib {
                StdlibVariant::Vanilla => "unmarked",
                StdlibVariant::Vec2Nobox => "vec2nobox",
            }
        )
        .to_lowercase()
    }
}

const GLSL_VECS: [VecTy; 3] = [VecTy::Vec2, VecTy::Vec3, VecTy::Vec4];
const GLSL_SOURCES: [GlslSource; 3] = [
    GlslSource::Local,
    GlslSource::UniformField,
    GlslSource::HelperParam,
];
const GLSL_PRODUCTIONS: [GlslProduction; 5] = [
    GlslProduction::LiteralCtor,
    GlslProduction::CallResult,
    GlslProduction::PhiIf,
    GlslProduction::ArithAdd,
    GlslProduction::UserStructField,
];
const GLSL_CONSUMPTIONS: [GlslConsumption; 5] = [
    GlslConsumption::Bind,
    GlslConsumption::HelperArg,
    GlslConsumption::ReturnWhole,
    GlslConsumption::Intrinsic,
    GlslConsumption::GpuOnlyIntrinsic,
];
const GLSL_STDLIBS: [StdlibVariant; 2] = [StdlibVariant::Vanilla, StdlibVariant::Vec2Nobox];

const GLSL_AXES: [usize; 5] = [
    GLSL_VECS.len(),
    GLSL_SOURCES.len(),
    GLSL_PRODUCTIONS.len(),
    GLSL_CONSUMPTIONS.len(),
    GLSL_STDLIBS.len(),
];

fn glsl_shape_at(i: &[usize]) -> GlslShape {
    GlslShape {
        vec: GLSL_VECS[i[0]],
        source: GLSL_SOURCES[i[1]],
        production: GLSL_PRODUCTIONS[i[2]],
        consumption: GLSL_CONSUMPTIONS[i[3]],
        stdlib: GLSL_STDLIBS[i[4]],
    }
}

fn glsl_shapes() -> Vec<GlslShape> {
    let mut shapes: Vec<GlslShape> =
        pairwise_cover(&GLSL_AXES, |i| render_glsl(glsl_shape_at(i), 0).is_some())
            .into_iter()
            .map(|i| glsl_shape_at(&i))
            .collect();
    // The Part-1 miscompile's own triple, both markings: a marked `Vec2` local in
    // a shader body scalarized into reserved `<name>__<i>` identifiers.
    for stdlib in GLSL_STDLIBS {
        for vec in GLSL_VECS {
            shapes.push(GlslShape {
                vec,
                source: GlslSource::Local,
                production: GlslProduction::CallResult,
                consumption: GlslConsumption::Bind,
                stdlib,
            });
        }
    }
    shapes.sort();
    shapes.dedup();
    shapes
}

fn fmt_lit(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// What `print` renders on BOTH backends: LuaJIT's `%.14g` drops a zero fraction
/// and JS `Number::toString` never adds one, so the two agree on every dyadic
/// value the generator uses (verified, not assumed).
fn fmt_expected(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

struct GlslProgram {
    source: String,
    expected: String,
    /// The entry fn, for `rock shader`.
    entry: String,
    /// `Sampler2D` is not CPU-constructible, so a `@gpu_only` shape carries no
    /// `@test` and the luajit/JS rungs have nothing to observe. Recorded rather
    /// than silently skipped.
    cpu_observable: bool,
}

/// One program per shape. Top-level names carry `id` so the whole subset can sit
/// in one directory without colliding in Rock's single namespace.
///
/// Deliberately NO `rock_shader(<entry>)` registration: that global is installed
/// by the stone engine, so a bare `rock test` dies at `__rock_shader_register (a
/// nil value)` and every luajit rung would read as a backend failure. The
/// registration shape is its own corpus case
/// (`shader_registration_fn_value.rock`), observed on the emit-only surface.
fn render_glsl(shape: GlslShape, id: usize) -> Option<GlslProgram> {
    // Production and consumption both live inside the helper for `HelperParam`,
    // and the `@gpu_only` arm needs the entry's sampler + varying. No overlap.
    if shape.source == GlslSource::HelperParam
        && shape.consumption == GlslConsumption::GpuOnlyIntrinsic
    {
        return None;
    }
    let t = shape.vec;
    let comps = t.comps();
    let sum: f64 = comps.iter().sum();
    let dot: f64 = comps.iter().map(|c| c * c).sum();

    let entry = format!("probe_{id}_frag");
    let sum_fn = format!("probe_sum_{id}");
    let pass_fn = format!("probe_pass_{id}");
    let body_fn = format!("probe_body_{id}");
    let hold_fn = format!("probe_hold_{id}");
    let u_ty = format!("ProbeU{id}");
    let holder_ty = format!("Holder{id}");

    let sum_of = |v: &str| -> String {
        t.fields()
            .iter()
            .map(|f| format!("{v}.{f}"))
            .collect::<Vec<_>>()
            .join(" + ")
    };

    let src = match shape.source {
        GlslSource::Local => t.literal(),
        GlslSource::UniformField => "u.uA".to_string(),
        GlslSource::HelperParam => "p".to_string(),
    };

    let produce = match shape.production {
        GlslProduction::LiteralCtor => {
            let inner: Vec<String> = t
                .fields()
                .iter()
                .map(|f| format!("{f}: ({src}).{f}"))
                .collect();
            format!("{} {{ {} }}", t.name(), inner.join(", "))
        }
        GlslProduction::CallResult => format!("{pass_fn}({src})"),
        GlslProduction::PhiIf => {
            format!("if ({src}).{} >= 0.0 {{ {src} }} else {{ {} }}", t.fields()[0], t.zero())
        }
        GlslProduction::ArithAdd => format!("{src} + {}", t.zero()),
        GlslProduction::UserStructField => format!("{hold_fn}({src}).inner"),
    };

    let (consume, expected_val) = match shape.consumption {
        GlslConsumption::Bind => (format!("let z = w\n        {}", sum_of("z")), sum),
        GlslConsumption::HelperArg => (format!("{sum_fn}(w)"), sum),
        GlslConsumption::ReturnWhole => (format!("{sum_fn}({pass_fn}(w))"), sum),
        GlslConsumption::Intrinsic => ("dot(w, w)".to_string(), dot),
        // `* 0.0` keeps the sampler read load-bearing for emission while leaving
        // the value the GPU rung would read unchanged.
        GlslConsumption::GpuOnlyIntrinsic => {
            (format!("{}(w) + texture(scene, tc).x * 0.0", sum_fn), sum)
        }
    };
    // A multi-statement consumption cannot be inlined into the `let s = ..` form.
    let consume_block = if consume.contains('\n') {
        let (head, tail) = consume.rsplit_once('\n').unwrap();
        format!("{head}\n        let s = {}", tail.trim_start())
    } else {
        format!("let s = {consume}")
    };

    let mut decls = String::new();
    if shape.source == GlslSource::UniformField {
        decls.push_str(&format!("value {u_ty} {{ uA: {} }}\n", t.name()));
    }
    if shape.production == GlslProduction::UserStructField {
        decls.push_str(&format!(
            "value {holder_ty} {{ inner: {ty} }}\n@pure fn {hold_fn}(q: {ty}) -> {holder_ty} {{ return {holder_ty} {{ inner: q }} }}\n",
            ty = t.name()
        ));
    }
    decls.push_str(&format!(
        "@pure fn {sum_fn}(q: {ty}) -> float {{ return {} }}\n@pure fn {pass_fn}(q: {ty}) -> {ty} {{ return q }}\n",
        sum_of("q"),
        ty = t.name()
    ));

    let mut params: Vec<String> = Vec::new();
    if shape.source == GlslSource::UniformField {
        params.push(format!("@uniform u: {u_ty}"));
    }
    if shape.consumption == GlslConsumption::GpuOnlyIntrinsic {
        params.push("scene: Sampler2D".to_string());
    }
    params.push("tc: Vec2".to_string());

    let body = if shape.source == GlslSource::HelperParam {
        decls.push_str(&format!(
            "@pure fn {body_fn}(p: {ty}) -> float {{\n        let w = {produce}\n        {consume_block}\n        return s\n}}\n",
            ty = t.name()
        ));
        format!("        let s = {body_fn}({})", t.literal())
    } else {
        format!("        let w = {produce}\n        {consume_block}")
    };

    let cpu_observable = shape.consumption != GlslConsumption::GpuOnlyIntrinsic;
    let cpu_args = if shape.source == GlslSource::UniformField {
        format!("{u_ty} {{ uA: {} }}, v(0.25, 0.5)", t.literal())
    } else {
        "v(0.25, 0.5)".to_string()
    };
    let expected = fmt_expected(expected_val);
    let test = if cpu_observable {
        format!(
            "\n@test(output = \"{expected}\")\nfn probe_{id}_answers() {{\n    let r = {entry}({cpu_args})\n    print(\"{{r.x}}\")\n}}\n"
        )
    } else {
        String::new()
    };

    let source = format!(
        "// GLSL shape: {}\n{decls}\n@shader(fragment)\nfn {entry}({}) -> Vec4 {{\n{body}\n        return Vec4 {{ x: s, y: tc.x, z: 0.0, w: 1.0 }}\n}}\n{test}",
        shape.name(),
        params.join(", ")
    );

    Some(GlslProgram {
        source,
        expected,
        entry,
        cpu_observable,
    })
}

/// The GLSL rung's verdict on one shape, per profile, plus the CPU rungs on the
/// SAME file. Divergence between the three is the finding class.
#[test]
#[ignore = "opt-in: spawns one rock process per shape (`just shape-sweep`)"]
fn the_glsl_subset_agrees_across_backends() {
    let shapes = glsl_shapes();
    let dir = std::env::temp_dir().join("rock_shape_oracle_glsl");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut glsl_counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut lua_counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut js_counts: BTreeMap<Bucket, usize> = BTreeMap::new();
    let mut unrepresentable = 0usize;
    let mut rows: Vec<String> = Vec::new();
    let mut divergences: Vec<String> = Vec::new();
    let mut glsl_miscompiles: Vec<String> = Vec::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();

    for (i, shape) in shapes.iter().enumerate() {
        let Some(prog) = render_glsl(*shape, i) else {
            unrepresentable += 1;
            continue;
        };
        let name = shape.name();
        let file = dir.join(format!("{name}.rock"));
        std::fs::write(&file, &prog.source).unwrap();

        let glsl = run_glsl(&file, &prog.entry, shape.stdlib);
        *glsl_counts.entry(glsl.bucket).or_insert(0) += 1;
        if glsl.bucket == Bucket::Miscompile {
            glsl_miscompiles.push(format!(
                "{name}: {}\n    source:\n{}",
                glsl.detail,
                indent_block(&prog.source)
            ));
        }
        if glsl.bucket == Bucket::LoudReject {
            *reasons.entry(format!("[glsl] {}", glsl.detail)).or_insert(0) += 1;
        }

        let (lua, js) = if prog.cpu_observable {
            let lua = observe(&file, Surface::Luajit, shape.stdlib);
            let js = observe(&file, Surface::Js, shape.stdlib);
            *lua_counts.entry(lua.bucket).or_insert(0) += 1;
            *js_counts.entry(js.bucket).or_insert(0) += 1;
            if lua.bucket == Bucket::LoudReject {
                *reasons.entry(format!("[luajit] {}", lua.detail)).or_insert(0) += 1;
            }
            (Some(lua), Some(js))
        } else {
            (None, None)
        };

        let cell = |o: &Option<Observation>| -> &'static str {
            o.as_ref().map(|o| o.bucket.label()).unwrap_or("n/a")
        };
        rows.push(format!(
            "  {:<62} glsl={:<12} luajit={:<12} js={}",
            name,
            glsl.bucket.label(),
            cell(&lua),
            cell(&js)
        ));

        // A shape whose GLSL and CPU verdicts differ. Some of these are expected
        // (a `@gpu_only` call has no CPU image); each one still gets named, because
        // "expected" is a judgement and it has to be visible to be judged.
        if let (Some(lua), Some(js)) = (&lua, &js) {
            if glsl.bucket != lua.bucket || lua.bucket != js.bucket {
                divergences.push(format!(
                    "{name}\n    glsl:   {} -- {}\n    luajit: {} -- {}\n    js:     {} -- {}\n    expected value {:?}\n    source:\n{}",
                    glsl.bucket.label(),
                    glsl.detail,
                    lua.bucket.label(),
                    lua.detail,
                    js.bucket.label(),
                    js.detail,
                    prog.expected,
                    indent_block(&prog.source)
                ));
            }
        }
    }

    let mut report = format!(
        "\nGLSL-legal subset: {} shapes, {} unrepresentable (not a verdict)\n",
        shapes.len(),
        unrepresentable
    );
    report.push_str(&rows.join("\n"));
    report.push_str("\n\nGLSL (both profiles):");
    report.push_str(&bucket_table(&glsl_counts));
    report.push_str("\nluajit (same files):");
    report.push_str(&bucket_table(&lua_counts));
    report.push_str("\njs (same files):");
    report.push_str(&bucket_table(&js_counts));
    report.push_str("\nrejection reasons:\n");
    for (reason, n) in &reasons {
        report.push_str(&format!("  {n:>3}  {reason}\n"));
    }
    report.push_str(&format!(
        "\ncross-backend divergences: {}\n{}\n",
        divergences.len(),
        divergences.join("\n")
    ));
    println!("{report}");

    // Illegal GLSL emitted with no diagnostic is the Part-1 class and the reason
    // this rung exists. Nothing is pinned: there is no known-live instance.
    assert!(
        glsl_miscompiles.is_empty(),
        "{report}\nGLSL MISCOMPILES ({}):\n{}",
        glsl_miscompiles.len(),
        glsl_miscompiles.join("\n")
    );
    // Every divergence must be one of the recorded classes. An unrecognised one is
    // a finding: the same shape legal on one backend and not another.
    let unjudged: Vec<&String> = divergences
        .iter()
        .filter(|d| !KNOWN_GLSL_DIVERGENCES.iter().any(|k| d.contains(k)))
        .collect();
    assert!(
        unjudged.is_empty(),
        "{report}\nUNJUDGED cross-backend divergences ({}) -- classify each in \
         KNOWN_GLSL_DIVERGENCES:\n{}",
        unjudged.len(),
        unjudged
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Divergence classes this rung has SEEN and judged. Matched against the whole
/// rendered divergence block, so a substring of the diagnostic is enough.
const KNOWN_GLSL_DIVERGENCES: &[&str] = &[
    // A user `value` struct returned from a helper called inside a shader body:
    // GLSL rejects (tier 0 is float/int/bool/VecN only), luajit and JS both run it.
    // The intended legality difference, and the diagnostic names the type -- so it
    // is recorded, not a finding. `Holder` only survives to the emitter because
    // the ctor crosses a CALL; inlined at the definition it folds to the bare
    // vector and GLSL never sees a struct.
    "is not tier 0",
];
