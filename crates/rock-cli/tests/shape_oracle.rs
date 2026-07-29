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
//! - the pairwise SWEEP, generated here, behind `ROCK_SHAPE_SWEEP=1`. Samples
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
}

/// Which stdlib the process reads. `stdlib/*.rock` is read from disk at runtime,
/// so the marked variant is a patched COPY plus `ROCK_STDLIB_DIR` -- one binary,
/// two stdlibs, the marker the only variable (`benchmarks/nobox-vec2/README.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdlibVariant {
    Vanilla,
    Vec2Nobox,
}

fn marked_stdlib() -> PathBuf {
    let dir = std::env::temp_dir().join("rock_shape_oracle_stdlib_vec2_nobox");
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

/// `rock test` reports a failing `@test` on stdout and STILL EXITS 0 on the
/// skull backend (`SkullOutcome::TestFail` never increments `failed`), so the
/// classifier reads the report, not the status. The JS backend exits 1 for the
/// same input; the divergence is recorded in
/// `docs/adversarial-shape-testing-plan.md` S "Findings from M1".
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
        expect: Expect::LoudReject {
            names: "survived lowering",
        },
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
        expect: Expect::Miscompile {
            flip_when: "`nobox_binding_shape` gates on `values_first_class` \
                        (GLSL has no boxed repr) -- expect Correct",
        },
        gap: Some("GLSL locals inherit nobox repr (LIVE)"),
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
            "  {:<32} {:<14} {}\n",
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
const STORAGES: [Storage; 11] = [
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

/// Greedy pairwise (all-pairs) selection: every PAIR of axis values from two
/// different axes appears in at least one selected shape. Deterministic -- no
/// RNG, so a failing sweep replays exactly.
fn pairwise_shapes() -> Vec<Shape> {
    let axes: [usize; 5] = [
        REPRS.len(),
        STORAGES.len(),
        PRODUCTIONS.len(),
        CONSUMPTIONS.len(),
        NULLABILITIES.len(),
    ];
    let at = |shape: &Shape, axis: usize| -> usize {
        match axis {
            0 => REPRS.iter().position(|v| *v == shape.repr).unwrap(),
            1 => STORAGES.iter().position(|v| *v == shape.storage).unwrap(),
            2 => PRODUCTIONS
                .iter()
                .position(|v| *v == shape.production)
                .unwrap(),
            3 => CONSUMPTIONS
                .iter()
                .position(|v| *v == shape.consumption)
                .unwrap(),
            _ => NULLABILITIES
                .iter()
                .position(|v| *v == shape.nullability)
                .unwrap(),
        }
    };
    let build = |i: [usize; 5]| Shape {
        repr: REPRS[i[0]],
        storage: STORAGES[i[1]],
        production: PRODUCTIONS[i[2]],
        consumption: CONSUMPTIONS[i[3]],
        nullability: NULLABILITIES[i[4]],
    };

    let mut wanted: BTreeSet<(usize, usize, usize, usize)> = BTreeSet::new();
    for a in 0..axes.len() {
        for b in (a + 1)..axes.len() {
            for va in 0..axes[a] {
                for vb in 0..axes[b] {
                    wanted.insert((a, b, va, vb));
                }
            }
        }
    }

    let mut chosen: Vec<Shape> = Vec::new();
    // Each round walks the whole candidate lattice in index order and keeps the
    // first combination covering the most uncovered pairs. Bounded by the pair
    // count, which shrinks strictly every round.
    while !wanted.is_empty() {
        let mut best: Option<([usize; 5], usize)> = None;
        for i0 in 0..axes[0] {
            for i1 in 0..axes[1] {
                for i2 in 0..axes[2] {
                    for i3 in 0..axes[3] {
                        for i4 in 0..axes[4] {
                            let idx = [i0, i1, i2, i3, i4];
                            let mut gain = 0;
                            for a in 0..axes.len() {
                                for b in (a + 1)..axes.len() {
                                    if wanted.contains(&(a, b, idx[a], idx[b])) {
                                        gain += 1;
                                    }
                                }
                            }
                            if gain > best.map(|(_, g)| g).unwrap_or(0) {
                                best = Some((idx, gain));
                            }
                        }
                    }
                }
            }
        }
        let (idx, _) = best.expect("pairs remain but no candidate covers any");
        for a in 0..axes.len() {
            for b in (a + 1)..axes.len() {
                wanted.remove(&(a, b, idx[a], idx[b]));
            }
        }
        let shape = build(idx);
        let _ = at(&shape, 0);
        chosen.push(shape);
    }
    chosen
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
    // fn-value x field-call.
    for repr in [Repr::Unmarked, Repr::Nobox] {
        for nullability in NULLABILITIES {
            out.push(Shape {
                repr,
                storage: Storage::StructField,
                production: Production::LiteralCtor,
                consumption: Consumption::FnValueOffField,
                nullability,
            });
        }
    }
    // cast x user-shadowing is not a shape in these axes -- a user `cast` decl
    // is a whole-program property, so it stays a seed case
    // (`cast_name_hijack.rock`) rather than a generated axis value.
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
    let ctor7 = format!("P {{ {} }}", (l.ctor_args)(7, 11));
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
            format!("(maybe(seed(7)) ?? {ctor7})")
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
            decls.push(format!(
                "fn call_it(f: () -> {stored_ty}) -> {stored_ty} {{ return f() }}"
            ));
            body.push(format!(
                "{}let captured: {stored_ty} = {produced}",
                ind(depth)
            ));
            body.push(format!("{}let slot = call_it(() => captured)", ind(depth)));
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
                body.push(format!("{}let got: P = {place} ?? {ctor7}", ind(depth)));
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
const KNOWN_REJECTIONS: &[&str] = &[
    // Null refinement does not reach an INDEX place: `if xs[0] != null { xs[0].a }`
    // and the `Map` equivalent both reject, while the same shape through a FIELD
    // place refines. Recorded as a judgement, per the plan's M5: a queued
    // feature, not intended residue -- but a loud rejection, not a miscompile.
    "on non-struct type",
    // Correct diagnostics on shapes the generator can spell but that are
    // statically redundant once refinement has run.
    "is not nullable",
    "with null",
    "immutable local",
    "survived lowering",
    "cannot assign",
    "value struct",
    "`value` struct",
    // NOT the bare substring "nobox". Every marked-tier diagnostic opens with
    // "nobox value `P` ..", so a bare "nobox" entry classifies the WHOLE tier as
    // already-judged and the unclassified gate stops gating exactly where it
    // matters most. Found by mutation: renaming the "survived lowering" entry
    // left the sweep GREEN, with its 44 shapes silently reabsorbed under
    // "nobox". Keep every entry specific to one diagnostic.
    "cannot cross boxed call boundary",
    "not a mutable place",
    "requires a value struct",
    "must be a `value` struct",
    "FlatVec",
    "NativeBuffer",
    "Map key",
    "Map value",
    "GC-carrying",
    "does not exist",
];

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
fn marked_sweep_shapes() -> Vec<Shape> {
    let mut shapes: Vec<Shape> = sweep_shapes()
        .into_iter()
        .map(|mut s| {
            s.repr = Repr::Nobox;
            s
        })
        .collect();
    shapes.sort();
    shapes.dedup();
    shapes
}

fn gated_off() -> bool {
    if std::env::var("ROCK_SHAPE_SWEEP").is_err() {
        println!(
            "SKIPPED: the pairwise sweep spawns one rock process per shape. \
             Run with ROCK_SHAPE_SWEEP=1."
        );
        return true;
    }
    false
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

#[test]
fn the_pairwise_sweep_finds_no_unexpected_miscompile() {
    if gated_off() {
        return;
    }
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
fn the_marked_sweep_finds_no_unexpected_miscompile() {
    if gated_off() {
        return;
    }
    let run = run_sweep(
        "marked sweep (every shape forced to `nobox`)",
        &marked_sweep_shapes(),
        Surface::Luajit,
        "rock_shape_oracle_sweep_marked",
    );
    assert_sweep(&run, true);
}

// GLSL has NO generated rung, and that is a property of the target rather than a
// gap in this harness: the shapes above are user value structs over `int` leaves,
// while GLSL accepts only the tier-0 whitelist (`vecN`, `Mat4`) and turns the
// scalar ABI off entirely (`values_first_class`). There is no GLSL-legal image of
// the generated corpus to run. The GLSL oracle is therefore the SEED pair
// (`glsl_local_unmarked` / `glsl_local_vec2_marked`), which is where the live gap
// lives; widening it into a shader-shaped generator over `Vec2`/`Vec4` is A5's
// job, gated by `shader-validate` on both profiles.

/// The JS backend over the SAME generated shapes. It is a real rung, not a
/// placeholder -- but its verdict set differs on purpose: `--backend js` reports
/// `SKIP(reason)` for shapes its emitter does not cover yet (that histogram IS
/// the emitter worklist, `docs/js-backend-plan.md`), and a rejection there can be
/// an unbuilt emitter arm rather than a language rule. So this test gates on the
/// one thing that means the same on every backend: a program that compiles, runs
/// and answers wrong.
#[test]
fn the_pairwise_sweep_on_js_finds_no_unexpected_miscompile() {
    if gated_off() {
        return;
    }
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
