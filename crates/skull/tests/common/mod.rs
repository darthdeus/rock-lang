//! Shared skull codegen-test harness: parse + semantic_pass, then either a full
//! MIR emit against the real stdlib (`emit_mir` / `emit_module`) or a bare
//! single-module workspace (`ws_of`). Pulled into each `tests/*.rs` via
//! `mod common;`.

#![allow(dead_code)] // every test file includes this module; none uses all of it.

use bone_semantic::inference::Workspace;
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Panic if bone recorded any type/lowering/interface error. `semantic_pass`
/// returns `Ok` with per-body diagnostics recorded on the workspace, so a
/// fixture that stops typechecking (e.g. its helper name goes ambiguous
/// against a new stdlib overload) would otherwise flow on with missing
/// resolutions and turn shape assertions vacuously green.
fn assert_no_bone_errors(ws: &Workspace) {
    let errs: Vec<String> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect();
    assert!(errs.is_empty(), "bone recorded errors: {errs:#?}");
}

/// The messages bone recorded for `src`, without emitting.
///
/// [`try_emit_module`] deliberately PANICS on these (a bone error there means a
/// malformed fixture), so a test whose subject IS a bone-level rejection --
/// annotation-registry violations, for one -- cannot use it.
pub fn bone_rejections(src: &str) -> Vec<String> {
    bone_rejection_errors(src).into_iter().map(|e| e.message).collect()
}

/// [`bone_rejections`] with each error's location and labels.
pub fn bone_rejection_errors(src: &str) -> Vec<skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    // BOTH channels. `semantic_pass` returns `Err` for a violation it treats as
    // fatal (a struct-field annotation is one) and RECORDS the rest on the
    // workspace; reading only the recorded set turns the fatal ones into a
    // panic inside the helper rather than a rejection the caller can assert.
    let ws = match bone_semantic::semantic_passes::semantic_pass(&mut modules) {
        Ok(ws) => ws,
        Err(e) => return vec![e],
    };
    ws.inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .cloned()
        .collect()
}

/// Full MIR emit for `src` (the `<test>` module) through the skull pipeline
/// (lower -> inline -> cleanup -> emit). Stdlib is loaded so int/float casts and
/// `%`/arithmetic ops resolve to their stdlib helpers -- else the using fns fail
/// to lower and are silently skipped.
pub fn emit_mir(src: &str) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

/// [`emit_mir`] as a package's `rock test` program rooted at `src`'s `@test`s.
pub fn emit_mir_package_test(src: &str) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let tests = skull::module_tests_with_ids(&ws, mid)
        .expect("collect tests")
        .into_iter()
        .map(|(func, _)| skull_mir::mir::FuncRef { module: mid, func })
        .collect::<Vec<_>>();
    assert!(!tests.is_empty(), "package-test source has no @test roots");
    skull::mir_emit::emit_workspace_mir_for_package_test(&ws, mid, &tests).expect("emit")
}

pub fn emit_mir_with_inputs(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> skull::mir_emit::MirEmit {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws.comptime_inputs = inputs;
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

pub fn try_emit_mir_with_inputs(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<skull::mir_emit::MirEmit, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws.comptime_inputs = inputs;
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid)
}

/// A workspace of `src` (the `<test>` module) over the real stdlib -- for tests
/// that inspect stdlib declarations (e.g. `@intrinsic` annotations).
pub fn ws_with_stdlib(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Emitted Lua for `src` (the common case; `emit_mir(src).lua`).
pub fn emit_module(src: &str) -> String {
    emit_mir(src).lua
}

/// One fragment of a modules-v1 package fixture (plan 2.2: fragment = file,
/// logical module = its `mod` header). The FIRST fragment is the one emission
/// starts from, so it owns `main`.
pub struct V1Fragment<'a> {
    pub file: &'a str,
    pub src: &'a str,
}

/// A v1 package's workspace over the real stdlib (which stays legacy, as it is
/// in a real build). Each fragment parses under the v1 dialect and carries the
/// package tag the host's graph stamps, so its definitions register under their
/// DefKeys (`<package>::<module path>::<name>`) instead of bare names.
pub fn ws_v1_package(package: &str, fragments: &[V1Fragment]) -> Workspace {
    let mut modules = STDLIB.clone();
    for fragment in fragments {
        let mut module = skull_parser::parse_module_clean_with_dialect(
            fragment.src,
            Ustr::from(fragment.file),
            skull_parser::Dialect::V1,
        )
        .expect("parse");
        module.package = Some(skull_parser::PackageTag {
            id: 0,
            self_name: package.to_string(),
        });
        modules.push(module);
    }
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

/// Full MIR emit for a v1 package, rooted at its first fragment.
pub fn emit_v1_package(package: &str, fragments: &[V1Fragment]) -> skull::mir_emit::MirEmit {
    let ws = ws_v1_package(package, fragments);
    let mid = *ws
        .module_ids
        .get(&Ustr::from(fragments[0].file))
        .expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

/// Like [`try_emit_module`] but with `Workspace.comptime_inputs` set before
/// emission -- the seam the host CLI uses for `build_var`/`embed_string`
/// (docs/comptime-plan.md tier 1).
pub fn try_emit_module_with(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws.comptime_inputs = inputs;
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// [`try_emit_module_with`] with a caller-chosen module FILE path -- for
/// fixtures where embed resolution must see a real on-disk file location
/// (package-relative embeds).
pub fn try_emit_module_with_file(
    src: &str,
    file: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse"));
    let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws.comptime_inputs = inputs;
    let mid = *ws.module_ids.get(&Ustr::from(file)).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// Like [`emit_mir`] but surfaces the codegen `Result` (a rejection such as a
/// reserved synthetic-name collision) instead of `.expect`ing success. Semantic
/// errors still panic (a malformed test, not what these cases probe).
pub fn try_emit_module(src: &str) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// [`try_emit_module`] with identity storage enforced on the unpackaged
/// `<test>` module, as bone fixtures do: the program's package is not exempt.
pub fn try_emit_module_enforcing_identity_storage(src: &str) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(&mut modules)
        .expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// Like [`try_emit_module`] but with LuaJIT's frame-limit rejection OFF, so the
/// emitted Lua of a body the check rejects is still produced. The
/// boundary-agreement test needs it: without the Lua there is nothing to ask
/// `loadfile` about, and "the compiler rejected it" would be unfalsifiable.
pub fn emit_module_without_frame_limits(src: &str) -> String {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_full_with_shapes(
        &ws,
        mid,
        skull::mir_emit::ShapeOverrides {
            frame_limits: Some(false),
            ..Default::default()
        },
    )
    .map(|(emit, _cache)| emit.lua)
    .expect("emit with the frame-limit check off")
}

/// Whether the bundled LuaJIT can LOAD `prog` (parse only -- the chunk is never
/// run), and the parse error when it cannot. This is the oracle the frame-limit
/// model is measured against.
pub fn luajit_loads(prog: &str, tag: &str) -> Result<(), String> {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_load_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit")
        .arg("-e")
        .arg(format!(
            "local f, e = loadfile({:?}); if not f then io.stderr:write(e) os.exit(1) end",
            path.to_string_lossy()
        ))
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// The body of the emitted definition named (or prefixed by) `name`. `name` is
/// the rock identifier; the emitted definition is mangled
/// (`hot_int_int_int`), so the prefix is resolved against the
/// definitions the chunk actually carries.
///
/// Handles both emitted callee shapes: the global-lookup `function <name>(..)`
/// and the callee-bound factory `__rock_mk_<name> = function(..) return
/// function(..)`, whose body ends at the column-0 `end end` that closes the
/// inner function and the factory together. The body TEXT is identical in both
/// (the factory's parameters are named like the globals they replace), so a
/// test asserting body shape reads the same either way.
///
/// The NAME is resolved before the text is searched, which is what keeps the
/// bound shape honest. A hoisted `<name>__lambda0` stays a plain global while
/// its enclosing `<name>` moves into a factory, so a bare `function <name>`
/// scan would silently return the HOIST; and `forward_wide` prefixes
/// `forward_wide_indirect`, so a bare prefix scan would return whichever of the
/// two happened to keep the global spelling.
pub fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let resolved = resolve_definition(lua, name);
    let global = format!("\nfunction {resolved}(");
    let (start, terminator) = match lua.find(&global) {
        Some(at) => (at + 1, "\nend\n"),
        None => {
            let head =
                format!("\n{}{resolved} = function(", skull_codegen_lua::CALLEE_FACTORY_PREFIX);
            let at = lua
                .find(&head)
                .unwrap_or_else(|| panic!("definition `{resolved}` has neither shape in:\n{lua}"));
            (at + 1, "\nend end\n")
        }
    };
    let rest = &lua[start..];
    let end = rest.find(terminator).map(|e| e + terminator.len()).unwrap_or(rest.len());
    &rest[..end]
}

/// The emitted definition `name` names: an exact name wins, then the first
/// definition it prefixes WITHOUT crossing a synthetic `__` suffix (so `main`
/// resolves to `main`, never to `main__lambda0`), then any prefix match.
fn resolve_definition<'a>(lua: &'a str, name: &str) -> &'a str {
    let names = definition_names(lua);
    names
        .iter()
        .find(|n| **n == name)
        .or_else(|| {
            names
                .iter()
                .find(|n| n.starts_with(name) && !n[name.len()..].contains("__"))
        })
        .or_else(|| names.iter().find(|n| n.starts_with(name)))
        .copied()
        .unwrap_or_else(|| panic!("no definition of `{name}` in:\n{lua}"))
}

/// Every top-level definition's emitted name, in either shape, in EMISSION
/// order. Order matters: a caller resolving a prefix takes the first match, and
/// the bound shape splits the population between `function <name>(` globals and
/// `__rock_mk_<name> = function(` factories, so scanning one kind and then the
/// other would reorder the chunk.
pub fn definition_names(lua: &str) -> Vec<&str> {
    let factory_head = format!("\n{}", skull_codegen_lua::CALLEE_FACTORY_PREFIX);
    let mut names: Vec<(usize, &str)> = Vec::new();
    for (at, m) in lua.match_indices("\nfunction ") {
        let rest = &lua[at + m.len()..];
        names.push((at, &rest[..rest.find('(').expect("fn header has `(`")]));
    }
    for (at, m) in lua.match_indices(factory_head.as_str()) {
        let rest = &lua[at + m.len()..];
        // The binding pass (`F = __rock_mk_F(..)`) shares the prefix and is not
        // a definition; only a factory HEADER carries ` = function(` on its own
        // line.
        if let Some(end) = rest.find(" = function(") {
            if !rest[..end].contains('\n') {
                names.push((at, &rest[..end]));
            }
        }
    }
    names.sort_by_key(|(at, _)| *at);
    names.into_iter().map(|(_, name)| name).collect()
}

/// Every emitted definition whose name contains `needle`, in emission order.
/// The shape-aware replacement for counting `function <needle>` occurrences:
/// under the bound shape a definition's header reads `__rock_mk_<name> =
/// function(`, so a raw substring count reports zero and whatever was built on
/// it passes or fails for the wrong reason.
pub fn definitions_matching<'a>(lua: &'a str, needle: &str) -> Vec<&'a str> {
    definition_names(lua).into_iter().filter(|n| n.contains(needle)).collect()
}

/// The body of the single emitted function whose name contains `needle`.
/// Synthetic callables carry a generated suffix (`<mangled>__lambda0`) or a
/// hash (`__local_go_<hash>_inner_0`), so they are found by pattern rather
/// than spelled out. Panics if it is not unique -- an ambiguous match would
/// silently assert against the wrong body.
pub fn body_of_synthetic<'a>(lua: &'a str, needle: &str) -> &'a str {
    let names: Vec<&str> = definition_names(lua)
        .into_iter()
        .filter(|n| n.contains(needle))
        .collect();
    assert_eq!(names.len(), 1, "expected exactly one fn containing `{needle}`, got {names:?}");
    function_body(lua, names[0])
}

/// The full emitted (mangled) name of the top-level fn whose SOURCE name is
/// `src`, read out of emitted `lua`. Every non-exempt fn mangles by signature
/// (`free_loop` -> `free_loop`), so this recovers the base symbol from
/// which a test builds the synthetic names it owns (`<mangled>__lambda<k>`,
/// `<mangled>__step`, ...). The base symbol carries NO `__` (that prefix only
/// appears on the synthetic derivations, and for a `co fn` a `__step`/`__resume`
/// header can emit before the constructor), so skip `__`-bearing candidates.
pub fn mangled_name(lua: &str, src: &str) -> String {
    definition_names(lua)
        .into_iter()
        .find(|name| name.starts_with(src) && !name.contains("__"))
        .unwrap_or_else(|| panic!("no non-synthetic definition of `{src}` in:\n{lua}"))
        .to_string()
}

/// Like [`try_emit_module`] but ALSO loads `lib` as an extra stdlib module. The
/// emit path is deliberately lenient for stdlib bodies (a codegen parity gap
/// there becomes a silent `skipped`), so a case probing that leniency needs a
/// body the module-file check actually classifies as stdlib.
pub fn try_emit_with_stdlib_module(
    lib: &str,
    src: &str,
) -> Result<String, skull_parser::CompilerError> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(lib, Ustr::from("<stdlib:probe>")).expect("parse lib"));
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// The dialect the package at `dir` (relative to the repository root) declares
/// in its stick.toml, as the host selects it for that package's files.
pub fn package_dialect(dir: &str) -> skull_parser::Dialect {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(dir).join("stick.toml");
    rock_host::stick::StickConfig::from_path(&manifest).expect("package manifest").dialect()
}

/// Like [`try_emit_with_stdlib_module`], but `lib` is a source of the package
/// at `dir` (Stone, say), parsed in the dialect that package declares and, for
/// a v1 package, tagged with its name the way the host's graph tags it.
pub fn try_emit_with_package_module(
    dir: &str,
    lib: &str,
    src: &str,
) -> Result<String, skull_parser::CompilerError> {
    let dialect = package_dialect(dir);
    let mut modules = STDLIB.clone();
    let file = format!("{dir}/probe.rock");
    let mut module =
        skull_parser::parse_module_clean_with_dialect(lib, Ustr::from(file.as_str()), dialect).expect("parse lib");
    if dialect == skull_parser::Dialect::V1 {
        module.package = Some(skull_parser::PackageTag { id: 0, self_name: dir.to_string() });
    }
    modules.push(module);
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

/// Like [`ws_with_stdlib`] but ALSO loads `lib` as an extra stdlib module, so a
/// case needs a body the emission-root predicate classifies as LIBRARY (and
/// therefore prunable) rather than as own-project source.
pub fn ws_with_stdlib_module(lib: &str, src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(lib, Ustr::from("<stdlib:probe>")).expect("parse lib"));
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

pub fn ws_with_dependency_module(lib: &str, src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(
        skull_parser::parse_module_clean(lib, Ustr::from("/tmp/rock-mono-dependency/engine.rock"))
            .expect("parse lib"),
    );
    modules.push(
        skull_parser::parse_module_clean(src, Ustr::from("/tmp/rock-mono-project/main.rock"))
            .expect("parse"),
    );
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

/// The `FuncRef` of the single workspace fn named `name`. Panics unless there is
/// exactly one -- an ambiguous match would silently assert about another
/// declaration.
pub fn fn_ref(ws: &Workspace, name: &str) -> skull_mir::mir::FuncRef {
    let mut found = Vec::new();
    for m in ws.modules.iter() {
        for (fid, f) in m.functions.iter() {
            if f.name.as_str() == name {
                found.push(skull_mir::mir::FuncRef { module: m.id, func: fid });
            }
        }
    }
    assert_eq!(found.len(), 1, "expected exactly one fn named `{name}`, got {}", found.len());
    found[0]
}

/// Single-module workspace (no stdlib) for incremental / hot-reload tests.
pub fn ws_of(src: &str) -> Workspace {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<t>")).expect("parse");
    bone_semantic::semantic_passes::semantic_pass(&mut [module]).expect("semantic_pass")
}

/// Run `prog` under luajit, returning trimmed stdout (panicking with stderr on a
/// non-zero exit). `tag` disambiguates the temp file so parallel tests never
/// collide (nextest is process-per-test, but be safe under `cargo test` threads).
pub fn run_luajit(prog: &str, tag: &str) -> String {
    use std::process::Command;
    let path = std::env::temp_dir().join(format!("skull_test_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit exited non-zero.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim().to_string()
}

pub struct AblationGuard(#[allow(dead_code)] skull_mir::ablation::Scope);

impl AblationGuard {
    pub fn on() -> Self {
        Self::set(true)
    }

    pub fn off() -> Self {
        Self::set(false)
    }

    fn set(on: bool) -> Self {
        AblationGuard(skull_mir::ablation::scope(on))
    }
}

pub struct InlineOffGuard(#[allow(dead_code)] skull_transform::InlineOffGuard);

impl InlineOffGuard {
    pub fn on() -> Self {
        InlineOffGuard(skull_transform::inline_off_guard())
    }
}

pub struct AllocationReportGuard(#[allow(dead_code)] skull::allocation_report::Scope);

impl AllocationReportGuard {
    pub fn on() -> Self {
        AllocationReportGuard(skull::allocation_report::scope(true))
    }
}

pub struct AllocationActivationGuard(
    #[allow(dead_code)] skull::allocation_report::ActivationScope,
);

impl AllocationActivationGuard {
    pub fn on() -> Self {
        AllocationActivationGuard(skull::allocation_report::activation_scope(true))
    }
}
