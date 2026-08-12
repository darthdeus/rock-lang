//! Anti-rot ratchet for the `--backend js` test executor
//! (`docs/multi-backend-testing-plan.md` T1): pins a floor on the number of
//! suite `@test`s currently RUNNABLE (emitted + executed green under node)
//! over a small fixed corpus, so an emitter regression that silently shrinks
//! JS coverage goes red. Emitter growth raises the floor.
//!
//! Deliberately NOT a full-suite run (default nextest profile stays fast);
//! the corpus is `tests/intrinsics/` plus three suite files verified to
//! contain at least one node-runnable test each.

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn corpus() -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    [
        "tests/intrinsics",
        "tests/generics_advanced.rock",
        "tests/type_holes_essential.rock",
        "tests/type_inference_core.rock",
        "tests/js_loops_valuemodel.rock",
        "tests/js_collections.rock",
        "tests/js_closures.rock",
        "tests/js_match.rock",
        "tests/js_multivalue.rock",
        "tests/js_inline.rock",
        "tests/js_value_structs.rock",
        "tests/js_shared_tail.rock",
        "tests/closure_semantics.rock",
        "tests/closure_capture_semantics.rock",
        "tests/js_coroutines.rock",
        "tests/coroutines.rock",
        "tests/stackless_coroutines/phase1_linear.rock",
        "tests/stackless_coroutines/phase3_loops.rock",
        "tests/js_table_unset.rock",
        "tests/js_string_builder.rock",
        "tests/js_string_primitives.rock",
        "tests/js_parse_float.rock",
        "tests/js_sort.rock",
        "tests/js_format_specs.rock",
        "tests/js_misc_intrinsics.rock",
        "tests/js_try_call.rock",
        "tests/js_module_globals.rock",
        "tests/js_put.rock",
        "tests/js_log.rock",
        "tests/js_deepcopy.rock",
        "tests/js_inspect.rock",
        "tests/js_enum_boxes.rock",
        "tests/js_vec_helpers.rock",
        "tests/js_value_gc_eq.rock",
        "tests/js_map_eq.rock",
        "tests/js_trampoline_fallback.rock",
        "tests/engine_systems_test.rock",
        "tests/deco_v2_init_with_args.rock",
        // Now fully runs: `from_codepoint` gained a byte-faithful UTF-8 JS mapping
        // (chess's SRS-JSON decode reaches it via json.rock), so the multibyte
        // `chars()` round-trip test passes instead of skipping.
        "tests/strings_chars.rock",
        // Skip sentinel: a `@nomangle inline(lua)` metatable probe with no JS body
        // (inherently luajit-only) keeps the SKIP(reason) path exercised.
        "tests/vec_direct_ty_tag.rock",
        // A backend-sibling fn taken as a VALUE (`float_to_int` -> inline(js)
        // sibling): before the fn-value collapse + JS per-target selection these
        // skipped ("lowering gap: inline(lua) body"); now they run.
        "tests/function_ref_cross_module.rock",
        // FlatVec<value> flat-slot lowering (mag web tier): element access over
        // the flat Float32Array backing (push/get/set/sort/iterate). Failed at
        // runtime (NaN reads) before `ContainerKind::NativeBufferValue`.
        "tests/flat_vec_value.rock",
        "tests/flat_vec_sugar.rock",
        // The whole bit-op family renders on JS: LuaJIT's `bit` library and JS
        // bitwise operators agree on int32 coercion, 5-bit shift masking and a
        // signed result, so the negative/high-bit cases here are the pin.
        "tests/bitwise_operators.rock",
        // Reflection follows the LOGICAL form on JS too: an `embed` child rebuilt
        // from its flat leaves, physical dotted keys never shown, and declared
        // fields supplied by the per-type `__rock_inspect_ty` helper rather than
        // by a key walk (which a null field is absent from).
        "tests/embed_reflection.rock",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect()
}

#[test]
fn js_runnable_coverage_ratchet() {
    if !node_available() {
        eprintln!("SKIP js_runnable_coverage_ratchet: `node` not on PATH");
        return;
    }
    let summary = rock_cli::js_test::js_test_run(&corpus(), false).expect("js test run");

    assert_eq!(summary.unit_failures, 0, "no unit should fail to build/run");
    assert_eq!(summary.tests_failed, 0, "no runnable test should FAIL under node");

    // The ratchet floor: node-executed green tests over this corpus. RAISE this
    // when emitter growth (T2+) makes more tests runnable; a drop below it is a
    // silent-coverage regression. T2 (strings + print + number formatting)
    // raised it from 4 to 55; V1's value-test corpus
    // (tests/intrinsics/value_reference.rock) raised it to 67; T3 (loops) added
    // tests/js_loops_valuemodel.rock to the corpus, raising it to 74; T4
    // (error/assert, Vec, structs, Map, equality) added tests/js_collections.rock
    // and lifted the whole corpus to 91. T5 (closures/lambdas) added
    // tests/js_closures.rock and lifted it to 99. T6 (short-circuit fold +
    // exhaustive-match structuring) added tests/js_match.rock and lifted it to 106.
    // T7 (multi-value ABI + loop-capture) added tests/js_multivalue.rock -> 111.
    // T8a (shared transform pipeline: inline/cleanup/recover + narrowed arity
    // guard + relooper empty-forward/infinite-loop fixes) added tests/js_inline.rock
    // -> 114 (full-suite pass count 2299 -> 2568). T8b (GC-free value structs emit
    // a plain `{__ty, ...}` object; GC-carrying ones reject) added
    // tests/js_value_structs.rock -> 118 (full-suite 2568 -> 2650). T8c
    // (shared-branch-tail structuring: lambda-body loop analysis, tail
    // duplication, labeled loops for multi-level jumps) added
    // tests/js_shared_tail.rock -> 122 (full-suite 2650 -> 2738). S1 (shared
    // closure-conversion MIR pass: env records + hoisted siblings, deleting the
    // T5/T7/T8c native-arrow path) added tests/closure_semantics.rock +
    // tests/closure_capture_semantics.rock -> 134; the loop-counter/enumerate
    // captures the deleted T7 path skipped now emit (full-suite 2743 -> 2756).
    // S4a (coroutines: shared step transform + materialize_frame + trampoline
    // emission + JS ctor/runtime) added tests/js_coroutines.rock,
    // tests/coroutines.rock, and stackless phase1/phase3 -> 157 (full-suite
    // 2756 -> 2876; the `resume`/`kill`/`coroutine fn` buckets collapsed).
    // T8d table_unset (`__table_unset`: Map delete / array hole, dispatched on
    // `instanceof Map`) added tests/js_table_unset.rock -> 165 (full-suite
    // 2878 -> 2941; the table_unset bucket of 142 collapsed). T8d string builder
    // (sb_new/sb_push/sb_build -> `[]`/`.push`/`.join("")`) added
    // tests/js_string_builder.rock -> 171 (full-suite 2941 -> 2951; the sb_new
    // bucket of 44 collapsed). T8d string primitives (find/substring/unit_at/
    // upper/lower/string_char host templates + defaulted-arg padding; the
    // Layer-2 split/replace/trim/... co-emit) added
    // tests/js_string_primitives.rock -> 188 (full-suite 2951 -> 3049; the find
    // 37 / unit_at 30 / substring 16 buckets collapsed). T8d parse_float
    // (`__parse_float`: strict Lua-tonumber semantics, empty/non-numeric -> null)
    // added tests/js_parse_float.rock -> 197 (full-suite 3049 -> 3063; the
    // parse_float bucket of 28 collapsed). T8d sort (`__vec_sort`: sort the
    // 1-indexed `_data` slice in place, key_fn via __rock_call) added
    // tests/js_sort.rock -> 203 (full-suite 3063 -> 3075; the sort bucket of 34
    // collapsed). T8d format specs (__rock_fmt_pad/grouping/binary/percent +
    // format_float host templates, and Format/FormatDyn lowered through the
    // single-value C printf port __rock_sprintf with round-half-to-even) added
    // tests/js_format_specs.rock -> 212 (full-suite 3075 -> 3190; the
    // __rock_fmt_* / Format / FormatDyn buckets collapsed). T8d misc intrinsics
    // (tuple_first/tuple_second -> 0-based array projection, __range -> a
    // Vec-shaped runtime object, is_release_mode -> false) added
    // tests/js_misc_intrinsics.rock -> 217 (full-suite 3190 -> 3209). T8d
    // try_call (`__try_call`: try/catch over __rock_call) + the checked-index
    // side-effect fix (an unused Vec/NativeBuffer OOB read is emitted, not
    // dropped, so its bounds check throws) added tests/js_try_call.rock -> 222
    // (full-suite 3209 -> 3218; the try_call bucket collapsed and 4 bounds tests
    // that would have gone red are green). T8d module globals (module-level
    // `let`/`global` STATE renders into a scoped `__G` object with each
    // initializer run once at load in dependency order; a top-level fn used as a
    // VALUE co-emits and renders as its JS name; a global populated by decorator
    // application rejects, since the per-test path does not emit module-load
    // decorator side effects) added tests/js_module_globals.rock -> 228
    // (full-suite 3223 -> 3421; the Global/StoreGlobal spread and fn-value reads
    // collapsed). T8d put (the test-local `io.write` idiom -> a harness `put`
    // global appending raw to the same buffer as `print`) added tests/js_put.rock
    // -> 231. (The 14 histogram `put` tests are native_buffer_value_* blocked
    // downstream on `__nb_alloc_<value>` custom C-struct alloc, a divergence
    // class; `put` is no longer their blocker.) T8d logging (the log_once /
    // log_throttled dedup+throttle `@nomangle inline(lua)` impls -> JS runtime
    // ports of the same name, `is_host_log_fn`; state in `__G.__stone_log_level`
    // + bare dedup/throttle vars; `__stone_emit` prints `[cat] msg` into the
    // shared buffer) added tests/js_log.rock -> 237 (full-suite 3424 -> 3447; the
    // log_reset_dedup bucket of 16 collapsed). T8d deepcopy (the recursive
    // `__rock_deepcopy_value` clone -> a JS runtime with a `seen` Map: shared refs
    // copy once, cycles terminate; TypedArray `.slice()`, Map/array element-wise,
    // structs/env-records/boxes own-key; primitives + `__fn` by identity so
    // closure shared-box + cofn cyclic-env-record aliasing survive) added
    // tests/js_deepcopy.rock -> 242 (full-suite 3447 -> 3459; the deepcopy bucket
    // of 8 collapsed). T8d inspect (the `__rock_inspect_value` pretty-printer ->
    // JS runtime: structs sorted-key ` = ` compact/multiline, Vec/Map/array,
    // scalars, fn-records as `function: ` leaves; a VALUE STRUCT or ENUM arg
    // rejects at the box boundary -- colon-cdata / numeric-tag renders the
    // scalarized JS shape cannot reproduce, deferred to those buckets) added
    // tests/js_inspect.rock -> 249 (full-suite 3459 -> 3485; the inspect /
    // inspect_formatted buckets collapsed, minus the value-struct/enum skips).
    // T8e boxed enums (MakeEnum/MakeEnumDyn box as `{__variant, __ty, 0:.., ..}`
    // via the nil-dropping `__rock_make_enum` ctor; unit variants of a mixed enum
    // are `__rock_enum_<E>_<V>` singletons; payload-free enums keep the int repr
    // with a `__rock_enum_<E>_names` Map; the coroutine channel rebox goes 0-based)
    // added tests/js_enum_boxes.rock -> 257 (full-suite 3492 -> 3537; the MakeEnum
    // / MakeEnumDyn / __rock_enum_* buckets collapsed + chained JsonValue/recursive/
    // record-variant unlocks). The inspect gate lift (a boxed enum inspects
    // faithfully: the walker recovers `E::V` and displays 0-based positional keys
    // as luajit's 1-based `[N]`, byte-matching; value-struct inspect stays gated)
    // added 4 byte-checked inspect cases -> 261 (full-suite 3537 -> 3541).
    // T8f Vec-helper coverage (per-family byte-checked cases -- index read/write
    // first+last, push/pop, sort, remove_at/swap_remove/clear drain, deepcopy,
    // inspect, for-in + enumerate, Vec in an enum payload and a struct field)
    // added tests/js_vec_helpers.rock -> 274 (13 runnable-green; suite pass count
    // unchanged, this slice adds coverage only -- the 0-based `_data` repr change
    // T8f originally scoped was falsified as JS-crate-only, see the plan entry).
    // T8g GC-carrying value structs (lift the MakeValueStruct GC reject -> plain
    // `{__ty,..}` object; per-type `__rock_vt_eq` registry mirroring the Lua
    // `_vt.__eq` field split -- Vec/Map/ref-struct/generic/string fields `===`,
    // scalar/nested-value fields `__op_eq`; `__op_eq` consults it before the
    // generic key walk; the GC-carrying `inspect` arm lifts, oracle-verified to
    // walk equals-format like the JS object, the GC-free/cdata arm stays gated)
    // added tests/js_value_gc_eq.rock -> 286 (12 runnable-green: 8 eq + 4 inspect;
    // full-suite pass 3554 -> 3593, GC-carrying reject bucket 73 -> 0). T8g(d)
    // Map eq hole (JS `__op_eq` had no Map branch -- `Object.keys(map)` is `[]`, so
    // unequal Maps behind an eq walk compared EQUAL; added a size + per-key `.get`
    // branch, order-free) added tests/js_map_eq.rock -> 291. T8h trampoline
    // fallback (a body the structurer rejects with the shared-branch-tail reason
    // -- a nested-loop shared tail or an over-cap tail -- re-emits through the
    // block trampoline `for(;;) switch(__bb)`, which accepts arbitrary CFGs;
    // cross-block materialized temps hoist to fn scope; the structured multi-value
    // return arm is ported into the trampoline term printer, ordinary-body path
    // only) added tests/js_trampoline_fallback.rock -> 300 (the shared-branch-tail
    // bucket 115 -> 0 suite-wide; 3 corpus shared-tail tests + the 6 new cases).
    // M1 program emission (module-driven `emit_program`: deco def fns + APPLICATION
    // lines at module load + ginit topo, the per-test path lifting the
    // decorator-populated-global skip via a whole-file emission when a test reads
    // such a global) added tests/engine_systems_test.rock (6) +
    // tests/deco_v2_init_with_args.rock (2) -> 308 (full-suite pass 3724 -> 3740,
    // decorator-populated bucket 16 -> 0; `rock run --backend js` gated separately
    // by crates/rock-cli/tests/js_run_gate.rs).
    // M2 intrinsics (MapNext cursor-cached keys-iterator shim; RNG mulberry32 +
    // env_var driver glue) mapped every unmapped surface the M1 corpus still
    // exercised, dropping it to 0 skips -- so tests/strings_chars.rock was added
    // (2 pass + 1 always-skip `from_codepoint`) to keep the SKIP path alive,
    // lifting the corpus to 312 (full-suite pass 3740 -> 3835, MapNext 79 /
    // random* 9 / env_var 2 skip buckets -> 0).
    // fn-value backend-sibling collapse (bone) + JS per-target fn-value selection
    // (skull `discover_globals`) let tests/function_ref_cross_module.rock's 2
    // `float_to_int`-as-value tests run (were 2 skips) -> 314.
    // Chess web tier (Arc 3): a byte-faithful `from_codepoint` JS mapping lets
    // tests/strings_chars.rock's multibyte test run (was the skip sentinel) -> 315;
    // tests/vec_direct_ty_tag.rock joins as the replacement skip sentinel (0 pass).
    // mag web tier (Arc 4): value-element NativeBuffer flat-slot lowering
    // (`ContainerKind::NativeBufferValue`) makes tests/flat_vec_value.rock +
    // tests/flat_vec_sugar.rock run green (were runtime-FAIL, kept out) -> 319.
    // The floor then went stale: `field_get`/`field_set` moving to stdlib/js.rock
    // lifted the SAME corpus to 346 without a bump. The bit-op render arms add
    // tests/bitwise_operators.rock's 14 on top -> 360. Per-type `__rock_inspect_ty`
    // dispatch (logical reflection over flat `embed` storage, plus the value-struct
    // `: ` tier) adds tests/embed_reflection.rock's 8, and lets one previously
    // physical-shaped render in the existing corpus pass -> 369.
    const FLOOR: usize = 369;
    assert!(
        summary.tests_passed >= FLOOR,
        "JS-runnable coverage regressed: {} passed < floor {}. Skip histogram:\n{}",
        summary.tests_passed,
        FLOOR,
        summary
            .skip_reasons
            .iter()
            .map(|(r, c)| format!("  {c:>4}  {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The corpus must keep exercising the SKIP path too (an empty histogram
    // here would mean the corpus no longer covers unmapped surface).
    assert!(
        !summary.skip_reasons.is_empty(),
        "corpus produced no skips -- it no longer exercises the SKIP(reason) path"
    );
}

#[test]
fn js_cofn_detached_lambda_is_executed() {
    if !node_available() {
        eprintln!("SKIP js_cofn_detached_lambda_is_executed: `node` not on PATH");
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let path = root.join("tests/closure_capture_semantics_cofn.rock");
    let paths = vec![path.to_string_lossy().into_owned()];
    let summary = rock_cli::js_test::js_test_run(&paths, false).expect("js test run");

    assert_eq!(summary.unit_failures, 0, "detached closure unit must build");
    assert_eq!(summary.tests_failed, 0, "no detached closure test may fail");
    assert_eq!(summary.tests_skipped, 0, "detached closures must emit on JS: {:?}", summary.skip_reasons);
    assert_eq!(summary.tests_passed, 12);
}
