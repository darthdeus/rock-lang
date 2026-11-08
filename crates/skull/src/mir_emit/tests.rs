use super::*;

#[cfg(test)]
mod lua_definition_scope_tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn luajit_loads(lua: &str) -> Option<(bool, String)> {
        let mut child = Command::new("luajit")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        if let Err(error) = child.stdin.take().unwrap().write_all(lua.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let output = child.wait_with_output().unwrap();
        Some((
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    #[test]
    fn definition_scopes_cross_the_luajit_main_constant_limit() {
        let mut raw = String::new();
        let mut definitions = LuaDefinitionBuffer::new();
        for i in 0..32_769 {
            let function = format!("function generated_{i}() return {i} end\n");
            raw.push_str(&function);
            definitions.push_str(&function);
        }
        let Some((raw_loaded, raw_error)) = luajit_loads(&raw) else {
            return;
        };
        assert!(!raw_loaded, "the unpartitioned control unexpectedly loaded");
        assert!(
            raw_error.contains("more than 65536 constants"),
            "the control failed for the wrong reason: {raw_error}"
        );

        let mut scoped = String::new();
        definitions.append_to(&mut scoped);
        scoped.push_str("assert(generated_0() == 0 and generated_32768() == 32768)\n");
        let (scoped_loaded, scoped_error) = luajit_loads(&scoped).unwrap();
        assert!(
            scoped_loaded,
            "partitioned definitions did not load: {scoped_error}"
        );
    }

    #[test]
    fn small_definition_population_keeps_its_exact_text() {
        let source = "function one() return 1 end\nfunction two() return one() + 1 end\n";
        let mut definitions = LuaDefinitionBuffer::new();
        definitions.push_str(source);
        let mut out = String::new();
        definitions.append_to(&mut out);
        assert_eq!(out, source);
    }
}

#[cfg(all(test, not(feature = "sequential")))]
mod setup_parallel_tests {
    use super::*;
    use bone_core::prelude::Instant;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    /// How long a setup branch waits for its peer to start before calling the
    /// branches serialized. Generous so a loaded gate host still passes.
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(5);

    /// A two-party rendezvous. Each branch announces that it started, then waits
    /// (bounded) for the other to announce the same, so overlap is proven by both
    /// sides observing each other rather than by the host happening to schedule
    /// the peer before the first branch finished.
    struct StartRendezvous {
        started: Mutex<usize>,
        both_started: Condvar,
    }

    impl StartRendezvous {
        fn new() -> Self {
            Self {
                started: Mutex::new(0),
                both_started: Condvar::new(),
            }
        }

        /// Returns once both branches have arrived; panics with a clear message
        /// if the peer did not start within `RENDEZVOUS_TIMEOUT`, which can only
        /// happen if the branches were serialized.
        fn wait_for_peer(&self, branch: &str) {
            let mut started = self.started.lock().expect("rendezvous state");
            *started += 1;
            self.both_started.notify_all();
            let deadline = Instant::now() + RENDEZVOUS_TIMEOUT;
            while *started < 2 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (guard, _) = self
                    .both_started
                    .wait_timeout(started, remaining)
                    .expect("rendezvous wait");
                started = guard;
            }
            let peer_started = *started >= 2;
            drop(started);
            assert!(
                peer_started,
                "setup branch {branch} started but its peer never did within {RENDEZVOUS_TIMEOUT:?}: \
                 the setup branches ran one after the other instead of concurrently"
            );
        }
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn independent_setup_branches_can_overlap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("two-thread test pool");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let rendezvous = Arc::new(StartRendezvous::new());
        pool.install(|| {
            parallel_join(
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    let rendezvous = Arc::clone(&rendezvous);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        rendezvous.wait_for_peer("left");
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
                {
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    let rendezvous = Arc::clone(&rendezvous);
                    move || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        rendezvous.wait_for_peer("right");
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                },
            );
        });
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "both setup branches must be active at once through parallel_join"
        );
    }
}

#[cfg(test)]
mod missing_owner_tests {
    use super::*;
    use bone_semantic::inference::Workspace;
    use skull_mir::mir::FuncRef;
    use skull_mir::monoty::MonoTy;
    use skull_parser::CompilerError;
    use ustr::Ustr;

    const SRC: &str = r#"
@inline
fn wrap<T>(x: T) -> T { return x }

fn top() -> int { return wrap(7) }
fn main() { let result = top() }
global RESULT: int = top()
"#;

    const CENSUS_SRC: &str = r#"
@inline
fn straight(flag: bool) -> bool {
    let unused = 99
    return flag
}

@inline
fn general(flag: bool) -> int {
    if flag { return 1 }
    return 2
}

fn top() -> int {
    let flag = straight(true)
    return general(flag)
}

fn main() { let result = top() }
global RESULT: int = top()
"#;

    const CROSS_BLOCK_SRC: &str = r#"
@inline
fn identity(flag: bool) -> bool { return flag }

fn run(select: bool) -> bool {
    let value = identity(true)
    if select { return value }
    return value
}

fn main() { let result = run(true) }
"#;

    const BOUNDED_SRC: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let sum = 0
    for value in it { sum = sum + value }
    return sum
}

fn top() -> int { return total([1, 2, 3].iter()) }
fn main() { let result = top() }
"#;

    const COROUTINE_PLAN_SRC: &str = r#"
enum Ev { Tick(string) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let counter = 100
    let add = (n: int) => n + counter
    R = add(5)
    yield Ev::Tick("x")
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

    const CLOSURE_SRC: &str = r#"
@inline
fn bump(x: int) -> int { return x }

fn top() -> int {
    let counter = 100
    let add = (n: int) => bump(n) + counter
    return add(5)
}

fn main() { let result = top() }
"#;

    const SPECIALIZED_SRC: &str = r#"
@inline
fn bump(x: int) -> int { return x }

fn generic<T>(x: T) -> int { return bump(1) }
fn top() -> int { return generic(7) }
fn main() { let result = top() }
"#;

    // This publisher-only type first reaches the shared interner in the setup
    // join; using it in a Rock body would intern it during concrete planning.
    const SETUP_SRC: &str = r#"
struct SetupIdentity<T> { value: T }

@nomangle inline(lua)
fn publish_setup_identity(value: SetupIdentity<int>) -> SetupIdentity<int> = "return value"

@nomangle inline(lua) fn publish(value: int) -> int = "return value"

@shader(fragment)
fn frag(tc: Vec2) -> Vec4 { return v(tc.x, 0.0, 0.0, 1.0) }

fn main() {
    let shader = rock_shader(frag)
    let published = publish(7)
}
"#;

    static STDLIB: std::sync::LazyLock<Vec<skull_parser::Ast>> = std::sync::LazyLock::new(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|file| {
                let id = Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str());
                skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
            })
            .collect()
    });

    static INLINE_OFF: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static INLINE_CENSUS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[allow(dead_code)]
    struct InlineOff(skull_transform::InlineOffGuard);

    impl InlineOff {
        fn on() -> Self {
            Self(skull_transform::inline_off_guard())
        }
    }

    #[allow(dead_code)]
    struct InlineOn(std::sync::MutexGuard<'static, ()>);

    impl InlineOn {
        fn on() -> Self {
            Self(INLINE_OFF.lock().unwrap_or_else(|e| e.into_inner()))
        }
    }

    #[allow(dead_code)]
    struct InlineCensusOn(std::sync::MutexGuard<'static, ()>, Option<String>);

    impl InlineCensusOn {
        fn on() -> Self {
            let guard = INLINE_CENSUS.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("ROCK_LOG").ok();
            unsafe { std::env::set_var("ROCK_LOG", "debug") };
            Self(guard, previous)
        }
    }

    impl Drop for InlineCensusOn {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => unsafe { std::env::set_var("ROCK_LOG", value) },
                None => unsafe { std::env::remove_var("ROCK_LOG") },
            }
        }
    }

    fn workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(SRC, Ustr::from("<missing-owner-test>"))
                .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn census_workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(CENSUS_SRC, Ustr::from("<inline-census-test>"))
                .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn cross_block_workspace() -> Workspace {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                CROSS_BLOCK_SRC,
                Ustr::from("<inline-cross-block-test>"),
            )
            .expect("parse"),
        ];
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn global_init_target_rejects_an_unresolved_storage_plan() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                "global G: int = 0\nfn main() {}",
                Ustr::from("<global-plan-test>"),
            )
            .expect("parse"),
        ];
        let mut ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|module| module.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let gid = ws
            .module(mid)
            .globals
            .iter()
            .find(|(_, global)| global.name == Ustr::from("G"))
            .map(|(gid, _)| gid)
            .expect("global");
        let ty = ws.module(mid).globals[gid].ty;
        let bone_semantic::prelude::TyId::Static(_, idx) = ty else {
            panic!("fixture global must have a static type id")
        };
        ws.module_mut(mid).globals[gid].ty = bone_semantic::prelude::TyId::Local(idx);
        let scx = skull_mir::lower::ScalarCtx::new(&ws);
        let cg = CodegenCtx::new(&ws);
        let global = &ws.module(mid).globals[gid];
        let err = global_init_target(&ws, &scx, &cg, global)
            .expect_err("a missing global representation plan must reject");
        assert!(
            err.message.contains("global type cannot be planned"),
            "{err}"
        );
    }

    fn bounded_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(BOUNDED_SRC, Ustr::from("<bound-plan-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn coroutine_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(
                COROUTINE_PLAN_SRC,
                Ustr::from("<coroutine-plan-test>"),
            )
            .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn closure_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(CLOSURE_SRC, Ustr::from("<closure-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn specialized_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(SPECIALIZED_SRC, Ustr::from("<specialized-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    fn setup_workspace() -> Workspace {
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(SETUP_SRC, Ustr::from("<setup-lane-test>"))
                .expect("parse"),
        );
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn candidate_repair_reads_the_concrete_publisher_index() {
        let (_, cache, _) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let canonical = cache.concrete.candidate_keys_by_func();
        let population: usize = canonical.values().map(|entries| entries.len()).sum();
        assert!(
            population > 0,
            "candidate-index fixture has no published entries"
        );
        assert_eq!(
            cache.candidate_index_by_func() as *const _,
            canonical as *const _,
            "repair must borrow the concrete publisher's index rather than a copied map"
        );
    }

    #[test]
    fn body_overlay_shares_the_published_code_artifact_handle() {
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<artifact-handle-test>"))
                .expect("parse")
        };
        let mut modules = vec![parse(SRC)];
        let ws1 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
        let mid = *ws1
            .module_ids
            .get(&Ustr::from("<artifact-handle-test>"))
            .unwrap();
        let (_, mut cache) = emit_full(&ws1, mid).expect("full v1");

        let changed = SRC.replace("return x", "let y = x\n return y");
        let mut modules = vec![parse(&changed)];
        let ws2 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
        let wrap = ref_for(&ws2, "wrap");
        let overlay = emit_overlay(
            &ws2,
            &mut cache,
            &[(wrap.module, BodyOwner::Function(wrap.func))],
        )
        .expect("overlay");
        assert!(
            std::sync::Arc::ptr_eq(&overlay.code_artifact, &cache.code_artifact),
            "the overlay result and cache must share one immutable CodeArtifact publication"
        );
        assert!(
            cache.repair_stats().code_artifact_assembly_reused,
            "overlay assembly duplicated the already-published replacement artifact"
        );
    }

    #[test]
    fn an_overlay_rebinds_the_callers_of_an_edited_body() {
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<overlay-binding-test>"))
                .expect("parse")
        };
        let edit = SRC.replace("return wrap(7)", "return wrap(9)");
        let mut modules = vec![parse(SRC)];
        let ws1 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
        let mid = *ws1
            .module_ids
            .get(&Ustr::from("<overlay-binding-test>"))
            .unwrap();
        let (_, mut cache) = emit_full(&ws1, mid).expect("full v1");

        let mut modules = vec![parse(&edit)];
        let ws2 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
        let top = ref_for(&ws2, "top");
        let produced = emit_overlay(
            &ws2,
            &mut cache,
            &[(top.module, BodyOwner::Function(top.func))],
        )
        .expect("overlay");
        // `top` inlines `wrap` and captures nothing, so it is a plain
        // definition; its caller `main` is rebound to the new closure.
        assert!(produced.lua.contains("function top_0("), "{}", produced.lua);
        assert!(
            produced.lua.contains("main = __rock_mk_main(top_0)"),
            "{}",
            produced.lua
        );
    }

    fn ref_for(ws: &Workspace, name: &str) -> FuncRef {
        ws.modules
            .iter()
            .filter(|module| module.id != ModuleId::builtin())
            .find_map(|module| {
                module
                    .functions
                    .iter()
                    .find(|(_, f)| f.name.as_str() == name)
                    .map(|(func, _)| FuncRef {
                        module: module.id,
                        func,
                    })
            })
            .expect("function")
    }

    fn concrete_symbol(ws: &Workspace, cache: &SkullEmitCache, fref: FuncRef) -> String {
        Instance::intern(cache.graph.interner(), fref, vec![MonoTy::Int])
            .symbol(ws, cache.scx.interner())
    }

    fn assert_residual_definition_and_call(
        ws: &Workspace,
        cache: &SkullEmitCache,
        emit: &MirEmit,
        callee: FuncRef,
    ) {
        let symbol = concrete_symbol(ws, cache, callee);
        // Definition headers in whichever callee shape the build emits: the
        // bound shape spells one `__rock_mk_<symbol> = function(`, and names the
        // symbol again as a factory parameter, which is a binding and not a
        // call.
        let factory = skull_codegen_lua::CALLEE_FACTORY_PREFIX;
        assert!(
            emit.lua.contains(&format!("function {symbol}("))
                || emit.lua.contains(&format!("{factory}{symbol} = function(")),
            "the concrete residual definition is missing: {symbol}\n{}",
            emit.lua
        );
        assert!(
            emit.lua.lines().any(|line| {
                !line.starts_with("function ")
                    && !line.starts_with(factory)
                    && line.contains(&format!("{symbol}("))
            }),
            "the residual call does not name the concrete definition: {symbol}\n{}",
            emit.lua
        );
    }

    fn emit_with_mutation(
        mutation: BodyMutation,
    ) -> Result<(Workspace, SkullEmitCache, MirEmit), CompilerError> {
        let ws = workspace();
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, cache) = FullEmit::program().mutation(mutation).run(&ws, mid)?;
        Ok((ws, cache, emit))
    }

    fn assert_rejection_or_residual(
        result: Result<(Workspace, SkullEmitCache, MirEmit), CompilerError>,
        callee: FuncRef,
    ) {
        match result {
            Err(err) => assert!(
                err.message
                    .contains("no concrete owner agreeing with its delivery instance"),
                "owner loss must be an explicit rejection, not an unrelated emission gap: {err:?}"
            ),
            Ok((ws, cache, emit)) => {
                assert_residual_definition_and_call(&ws, &cache, &emit, callee)
            }
        }
    }

    #[test]
    fn missing_generic_callee_owner_rejects_or_emits_concrete_residual() {
        let ws = workspace();
        let caller = ref_for(&ws, "top");
        let callee = ref_for(&ws, "wrap");
        assert_rejection_or_residual(
            emit_with_mutation(BodyMutation::RemoveCalleeOwner { caller, callee }),
            callee,
        );
    }

    #[test]
    fn incomplete_generic_callee_owner_key_rejects_or_emits_concrete_residual() {
        let ws = workspace();
        let caller = ref_for(&ws, "top");
        let callee = ref_for(&ws, "wrap");
        assert_rejection_or_residual(
            emit_with_mutation(BodyMutation::ClearCalleeOwnerKey { caller, callee }),
            callee,
        );
    }

    #[test]
    fn emission_rejects_a_poisoned_concrete_bound_plan() {
        let _inline = InlineOff::on();
        let ws = bounded_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "total");
        let err = match FullEmit::program()
            .mutation(BodyMutation::ClearBoundPlan { body })
            .run(&ws, mid)
        {
            Ok(_) => panic!("emission ignored the poisoned concrete bound plan"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete callee bound plan is empty for its site"),
            "the poisoned plan must fail at the plan consumer, got: {err:?}"
        );
    }

    #[test]
    fn emission_consumes_a_concrete_coroutine_result_plan() {
        let ws = coroutine_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<coroutine-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "worker");
        let (intact, intact_cache) = emit_full(&ws, mid).expect("intact emission");
        let symbol = Instance::base(body).symbol(&ws, intact_cache.scx.interner());
        assert!(
            intact.lua.contains(&format!("function {symbol}__advance(")),
            "the intact coroutine fixture must exercise the concrete yield channel"
        );
        let err = match FullEmit::program()
            .mutation(BodyMutation::ClearCoroutineResultPlan { body })
            .run(&ws, mid)
        {
            Ok(_) => panic!("emission ignored the poisoned coroutine result plan"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete callee signature disagrees with its body plan"),
            "the poisoned coroutine plan must fail closed at the concrete-plan join, got: {err:?}"
        );
    }

    #[test]
    fn emission_rejects_a_missing_concrete_iterator_witness_instance() {
        let _inline = InlineOff::on();
        let ws = bounded_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("test module");
        let body = ref_for(&ws, "total");
        let err = match FullEmit::program()
            .mutation(BodyMutation::ClearIteratorWitnessInstance { body })
            .run(&ws, mid)
        {
            Ok(_) => panic!("emission ignored the missing concrete iterator witness instance"),
            Err(err) => err,
        };
        assert!(
            err.message
                .contains("concrete ABI plan is missing for callee `next`"),
            "the missing iterator bound plan must fail at its production consumer, got: {err:?}"
        );
    }

    #[test]
    fn full_emission_deduplicates_abi_plan_derivations() {
        // Its own fixture: `helper` must be CALLED from more than one site, or
        // the callee question is asked once and a zero hit count cannot
        // distinguish "no duplicates" from "memo not consulted".
        const DEDUP_SRC: &str = r#"
fn helper(x: int) -> int { return x }
fn first() -> int { return helper(1) }
fn second() -> int { return helper(2) }
fn main() { let result = first() }
"#;
        let mut modules = vec![
            skull_parser::parse_module_clean(DEDUP_SRC, Ustr::from("<abi-dedup-test>"))
                .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, cache) = emit_full(&ws, mid).expect("intact emission");
        let stats = cache.scx.abi_plan_stats();
        let (hits, misses) = (stats.hits(), stats.requests() - stats.hits());
        assert!(
            misses > 0,
            "no ABI-plan question consulted the memo (hits={hits})"
        );
        assert!(
            hits > 0,
            "three helper call sites repeat the callee question; zero hits means the \
             production path is not consulting the memo (misses={misses})"
        );
        assert!(
            stats.stores > 0 && stats.unique_keys > 0,
            "derivations must commit to the table: {stats:?}"
        );
        assert!(
            stats.shared_lookups > 0,
            "the production population must reach the shared ABI table: {stats:?}"
        );
        assert_eq!(
            stats.shared_lookups,
            stats.shared_hits + stats.shared_misses,
            "shared lookup accounting lost a route: {stats:?}"
        );
        assert!(
            stats.published_hits > 0,
            "the production consumers must reach the immutable published plan table: {stats:?}"
        );
        assert_eq!(
            stats.published_misses, 0,
            "a published-table miss would fall back to the old shared route: {stats:?}"
        );
        assert_eq!(
            stats.plan_clones, 0,
            "cached plan consumers must share immutable handles rather than clone plans: {stats:?}"
        );
        assert_eq!(
            stats.cloned_param_slots, 0,
            "cached plan consumers must not clone parameter-slot vectors: {stats:?}"
        );
        assert!(
            stats.plan_size_samples() > 0
                && stats.plan_size_p50() <= stats.plan_size_p95()
                && stats.plan_size_p95() <= stats.plan_size_max,
            "plan-size census must have a nonzero, ordered population: {stats:?}"
        );
        assert_eq!(
            emit.abi_plan_stats, stats,
            "the reported stats must be the context's own counters"
        );
    }

    #[test]
    fn concrete_result_order_preserves_emitted_lua_bytes() {
        let _inline = InlineOn::on();
        let ws = workspace();
        let mid = ws
            .modules
            .iter()
            .find(|module| module.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (canonical, _) = FullEmit::program()
            .mutation(BodyMutation::ConcreteResultOrder(
                skull_graph::concrete_body::BuildResultOrder::Canonical,
            ))
            .run(&ws, mid)
            .expect("canonical emission");
        let (reverse, _) = FullEmit::program()
            .mutation(BodyMutation::ConcreteResultOrder(
                skull_graph::concrete_body::BuildResultOrder::Reverse,
            ))
            .run(&ws, mid)
            .expect("reverse emission");
        assert!(!canonical.lua.is_empty(), "fixture emitted no Lua artifact");
        assert_eq!(canonical.lua, reverse.lua);
    }

    #[test]
    fn abi_plan_publication_witness_catches_a_bypassed_publish() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                "fn helper(x: int) -> int { return x }\nfn main() { let result = helper(1) }",
                Ustr::from("<abi-publish-bypass-test>"),
            )
            .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (_, cache) = FullEmit::program()
            .mutation(BodyMutation::SkipAbiPlanPublish)
            .run(&ws, mid)
            .expect("the bypass must still take the fallback path");
        let stats = cache.scx.abi_plan_stats();
        let witness = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                stats.published_hits > 0,
                "the publication witness must fail when the publish step is bypassed: {stats:?}"
            );
        }));
        assert!(
            witness.is_err(),
            "the publication witness did not see its bypass: {stats:?}"
        );
    }

    #[test]
    fn full_emission_reports_pre_assembly_setup_work() {
        let (_, _, emit) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let setup = emit
            .phases
            .iter()
            .filter(|(name, _)| name.starts_with("setup: "))
            .collect::<Vec<_>>();
        assert_eq!(
            setup.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "setup: validation+context",
                "setup: root collection",
                "setup: graph",
                "setup: concrete plans",
                "setup: artifact+shader+ABI lanes",
                "setup: ABI validation",
                "setup: inline candidate table",
            ]
        );
        assert!(setup.iter().all(|(_, duration)| *duration > Duration::ZERO));
        // The joined group's children report under their own prefix so no
        // consumer can sum them into the sequential setup rows.
        let join = emit
            .phases
            .iter()
            .filter(|(name, _)| name.starts_with("join: "))
            .collect::<Vec<_>>();
        assert_eq!(
            join.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "join: cpu artifact branch",
                "join: shader view branch",
                "join: ABI plan warm branch"
            ]
        );
        assert!(join.iter().all(|(_, duration)| *duration > Duration::ZERO));
    }

    #[test]
    fn body_attribution_reports_nonzero_work_and_distribution() {
        let _inline = InlineOn::on();
        let (_, _, emit) = emit_with_mutation(BodyMutation::None).expect("intact emission");
        let attribution = &emit.attribution;
        assert!(
            attribution.bodies_examined > 0,
            "no emitted bodies were examined: {attribution:?}"
        );
        assert!(
            attribution.lower.bodies_with_work > 0 && attribution.lower.instructions_visited > 0,
            "lowering attribution has no production population: {attribution:?}"
        );
        assert!(
            attribution.inline.accepted_splices > 0
                && attribution.inline.copied_instructions > 0
                && attribution.inline.borrowed_block_scans > 0
                && attribution.inline.instructions_scanned > 0
                && !attribution.inline.top_owners.is_empty(),
            "inline attribution has no accepted splice population: {attribution:?}"
        );
        assert!(
            attribution.lower.p50_us > 0
                && attribution.lower.p50_us <= attribution.lower.p95_us
                && attribution.lower.p95_us <= attribution.lower.max_us,
            "lowering duration distribution is empty or unordered: {attribution:?}"
        );
        assert!(
            attribution.lower.concrete_mono_requests > 0
                && attribution.lower.concrete_mono_derivations > 0
                && attribution.lower.concrete_mono_requests
                    == attribution.lower.concrete_mono_cache_hits
                        + attribution.lower.concrete_mono_derivations
                && attribution.lower.concrete_mono_unique_ids
                    == attribution.lower.concrete_mono_derivations
                && attribution.lower.concrete_mono_cache_hits > 0
                && attribution.lower.concrete_mono_cache_insertion_cloned_nodes == 0
                && attribution.lower.concrete_mono_cache_hit_cloned_nodes == 0
                && attribution.lower.concrete_mono_cloned_nodes
                    == attribution.lower.concrete_mono_interner_miss_cloned_nodes
                        + attribution.lower.concrete_mono_cache_insertion_cloned_nodes
                        + attribution.lower.concrete_mono_cache_hit_cloned_nodes
                        + attribution
                            .lower
                            .concrete_mono_explicit_owned_escape_cloned_nodes
                && attribution.lower.concrete_mono_explicit_owned_escapes > 0
                && attribution.lower.concrete_mono_cache_hit_cloned_nodes == 0
                && attribution.lower.concrete_mono_cache_insertion_cloned_nodes == 0
                && attribution.lower.concrete_mono_shared_handle_requests
                    == attribution.lower.concrete_mono_requests
                && attribution
                    .lower
                    .concrete_mono_by_consumer
                    .iter()
                    .map(|stats| stats.requests)
                    .sum::<usize>()
                    == attribution.lower.concrete_mono_requests
                && attribution
                    .lower
                    .concrete_mono_by_consumer
                    .iter()
                    .map(|stats| stats.cloned_nodes)
                    .sum::<usize>()
                    == attribution.lower.concrete_mono_cloned_nodes
                && attribution.lower.mir_instruction_allocations > 0
                && attribution.lower.mir_instruction_capacity
                    >= attribution.lower.instructions_visited,
            "lowerer ownership census has no production population: {attribution:?}"
        );
        let consumers = &attribution.lower.concrete_mono_by_consumer;
        assert!(
            consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize].requests > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::CallParameter as usize]
                    .explicit_owned_escapes
                    == 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::GlobalPlan as usize]
                    .explicit_owned_escapes
                    > 0
                && consumers[skull_mir::lower::ConcreteMonoConsumer::Dispatch as usize].requests
                    > 0,
            "named lowerer consumer ownership routes are misclassified: {attribution:?}"
        );
        if std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug") {
            assert!(
                attribution.lower.concrete_mono_timed_requests > 0
                    && attribution.lower.concrete_mono_lookup_ns > 0
                    && attribution.lower.concrete_mono_clone_self_ns > 0,
                "opt-in lowerer timing has no measured population: {attribution:?}"
            );
            let reallocations = attribution.lower.mir_instruction_reallocations
                + attribution.lower.mir_cell_reallocations
                + attribution.lower.mir_block_reallocations;
            let reallocation_ns = attribution.lower.mir_instruction_reallocation_ns
                + attribution.lower.mir_cell_reallocation_ns
                + attribution.lower.mir_block_reallocation_ns;
            assert!(
                reallocations > 0 && reallocation_ns > 0,
                "opt-in MIR growth timing has no measured population: {attribution:?}"
            );
        }
    }

    #[test]
    fn inline_census_has_a_nonzero_production_population() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = census_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-census-test>"))
            .expect("census test module");
        let (emit, _) = emit_full(&ws, mid).expect("intact emission");
        let census = &emit.attribution.inline.census;
        assert!(
            census.candidate_requests > 0
                && census.candidate_requests
                    == census.candidate_cache_hits + census.candidate_derivations,
            "candidate requests must partition into hits and derivations: {census:?}"
        );
        assert_eq!(
            census.candidate_derivations,
            census.candidate_keys.len(),
            "each derivation must publish one measured instance key: {census:?}"
        );
        assert_eq!(
            census.candidate_derivations,
            census.candidate_worker_derivations.values().sum::<usize>(),
            "worker derivations must cover the global derivation population: {census:?}"
        );
        assert!(
            emit.attribution.inline.candidate_unique_keys > 0
                && census.candidate_requested_keys.len()
                    >= emit.attribution.inline.candidate_unique_keys
                && census.candidate_derivations == 0
                && emit.attribution.inline.candidate_setup_keys > 0
                && emit.attribution.inline.candidate_setup_warmed > 0,
            "the published candidate table must serve a nonzero requested population: {census:?}"
        );
        assert_eq!(
            emit.attribution.inline.candidate_setup_warmed,
            emit.attribution.inline.candidate_setup_eligible
                + emit.attribution.inline.candidate_setup_rejected,
            "candidate table warm results must partition into eligible and rejected keys"
        );
        assert!(
            emit.attribution.inline.candidate_setup_keys
                >= emit.attribution.inline.candidate_unique_keys,
            "the candidate table cannot be smaller than its requested-key population"
        );
        assert!(
            census.straight_accepted > 0 && census.general_accepted > 0,
            "both splice shapes must reach production: {census:?}"
        );
        assert!(
            census.substitution_invocations == 0
                && census.substitution_instructions_inspected == 0
                && census.substitution_terminators_inspected == 0
                && census.substitution_replacements == 0,
            "straight-result binding must remove the whole-caller substitution route: {census:?}"
        );
        assert_eq!(
            census.clone.source_instructions,
            census.clone.live_instructions + census.clone.dead_instructions,
            "instruction clone census must conserve source entries: {census:?}"
        );
        assert_eq!(
            census.clone.source_cells,
            census.clone.live_cells + census.clone.dead_cells,
            "cell clone census must conserve source entries: {census:?}"
        );
        assert_eq!(
            census.clone.source_blocks,
            census.clone.live_blocks + census.clone.dead_blocks,
            "block clone census must conserve source entries: {census:?}"
        );
        assert!(
            emit.attribution.inline.cleanup_live_before > 0
                && emit.attribution.inline.cleanup_live_after > 0
                && emit.attribution.inline.cleanup_live_after
                    <= emit.attribution.inline.cleanup_live_before,
            "cleanup census must expose a nonzero live population and cannot grow it: {:?}",
            emit.attribution.inline
        );
        assert!(
            census.clone.cloned_instructions <= census.clone.source_instructions
                && census.clone.cloned_cells <= census.clone.source_cells
                && census.clone.cloned_blocks <= census.clone.source_blocks,
            "cloning may not exceed the measured source population: {census:?}"
        );
        assert_eq!(
            census.clone.inst_remap_entries, census.clone.live_instructions,
            "every live instruction and no dead instruction must enter the remap: {census:?}"
        );
        assert_eq!(
            census.clone.cell_remap_entries, census.clone.live_cells,
            "every live cell and no dead cell must enter the remap: {census:?}"
        );
        assert_eq!(
            census.clone.block_remap_entries, census.clone.cloned_blocks,
            "only cloned live blocks may enter the remap: {census:?}"
        );
        assert!(
            census.clone.dead_instructions > 0,
            "the census fixture must expose a dead template instruction: {census:?}"
        );
        assert_eq!(
            census.clone.dead_cloned_instructions
                + census.clone.dead_cloned_cells
                + census.clone.dead_cloned_blocks,
            0,
            "the live template must not clone dead arena entries: {census:?}"
        );
        assert!(
            census.clone.inst_remap_capacity > 0
                && census.clone.cell_remap_capacity > 0
                && census.clone.block_remap_capacity > 0,
            "exact template populations must reach clone-map reservation: {census:?}"
        );
        assert!(
            census.clone.caller_reserved_instructions > 0
                && census.clone.caller_reserved_cells > 0
                && census.clone.caller_reserved_blocks > 0,
            "exact template populations must reach caller arena reservation: {census:?}"
        );
        macro_rules! side_bounds {
            ($field:ident) => {
                assert!(
                    census.clone.side_tables.$field.examined
                        <= census.clone.side_tables.$field.source_rows
                        && census.clone.side_tables.$field.copied
                            <= census.clone.side_tables.$field.examined,
                    "side-table census is not bounded for {}: {:?}",
                    stringify!($field),
                    census
                );
            };
        }
        side_bounds!(spec_bindings);
        side_bounds!(callee_instances);
        side_bounds!(abi_plans);
        side_bounds!(representation_plans);
        side_bounds!(closure_environment_plans);
        side_bounds!(raw_bound_trees);
        side_bounds!(resolved_bound_trees);
        side_bounds!(function_value_symbols);
        side_bounds!(instruction_spans);
        side_bounds!(call_sites);
        side_bounds!(call_site_owners);
        side_bounds!(iterator_headers);
    }

    #[test]
    fn inline_population_census_proves_each_lowering_route_and_conserves_keys() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = census_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-census-test>"))
            .expect("census test module");
        let (emit, _) = emit_full(&ws, mid).expect("intact emission");
        let population = &emit.attribution.inline.population;
        assert!(
            population.candidate_published_keys > 0
                && population.candidate_lower_attempted > 0
                && population.candidate_lowered_keys > 0
                && population.candidate_eligible_keys > 0
                && population.candidate_rejected_before_lower > 0
                && population.ordinary_consumer_keys > 0
                && population.always_lowered_units > 0
                && population.candidate_ordinary_consumer_overlap > 0
                && population.ordinary_canonical_reuse_keys
                    == population.candidate_ordinary_consumer_overlap
                && population.ordinary_canonical_miss_keys == 0
                && population.candidate_requested_unique_keys > 0
                && population.candidate_spliced_unique_keys > 0
                && population.candidate_requested_never_spliced > 0,
            "the production route fixture has no required population: {population:?}"
        );
        assert_eq!(
            emit.attribution.inline.candidate_setup_warmed,
            population.candidate_lower_attempted + population.candidate_rejected_before_lower,
            "candidate warm keys must partition into lower attempts and early rejections: {population:?}"
        );
        assert_eq!(
            population.candidate_lower_attempted,
            population.candidate_lowered_keys + population.candidate_lower_failed,
            "lower attempts must partition into successful lowers and lower failures: {population:?}"
        );
        assert_eq!(
            population.candidate_lowered_keys,
            population.candidate_eligible_keys + population.candidate_rejected_after_lower,
            "successful lowers must partition into eligible and post-lowering rejections: {population:?}"
        );
        assert!(
            population.candidate_lower_attempted >= population.candidate_lowered_keys
                && population.candidate_raw_instructions > 0
                && population.candidate_raw_cells > 0
                && population.candidate_raw_blocks > 0
                && population.candidate_cleaned_instructions > 0
                && population.candidate_cleaned_cells > 0
                && population.candidate_cleaned_blocks > 0
                && population.candidate_full_clone_ns > 0
                && population.candidate_compact_ns > 0
                && population.candidate_full_clone_capacity.0 > 0
                && population.candidate_full_clone_capacity.1 > 0
                && population.candidate_full_clone_capacity.2 > 0
                && population.candidate_compact_capacity.0 > 0
                && population.candidate_compact_capacity.1 > 0
                && population.candidate_compact_capacity.2 > 0
                && population.ordinary_lowering_derivations >= population.ordinary_lowered_keys
                && population.specialized_lowering_derivations
                    >= population.specialized_lowered_keys
                && population.keys_lowered_more_than_once == 0
                && population.first_lower_ns > 0
                && population.duplicate_lower_ns == 0
                && population.canonical_clone_derivations > 0
                && emit.attribution.inline.census.splice_ns > 0,
            "lowering overlap census has no measured duplicate population: {population:?}"
        );
        assert_eq!(
            population.candidate_published_keys,
            population.candidate_requested_unique_keys
                + population.candidate_published_never_requested,
            "published keys must partition into requested and never-requested keys: {population:?}"
        );
        assert_eq!(
            population.candidate_requested_unique_keys,
            population.candidate_spliced_unique_keys + population.candidate_requested_never_spliced,
            "requested keys must partition into spliced and unspliced keys: {population:?}"
        );
        assert_eq!(
            population.candidate_requested_not_published, 0,
            "a production inline lookup must use the published candidate table: {population:?}"
        );

        let specialized = specialized_workspace();
        let specialized_mid = *specialized
            .module_ids
            .get(&Ustr::from("<specialized-test>"))
            .expect("specialized test module");
        let (specialized_emit, _) =
            emit_full(&specialized, specialized_mid).expect("specialized emission");
        let specialized_population = &specialized_emit.attribution.inline.population;
        assert!(
            specialized_population.specialized_consumer_keys > 0
                && specialized_population.specialized_canonical_reuse_keys
                    == specialized_population.candidate_specialized_consumer_overlap
                && specialized_population.specialized_canonical_miss_keys == 0,
            "the specialized production route was not censused: {specialized_population:?}"
        );
    }

    #[test]
    fn canonical_profiles_survive_fresh_incremental_overlay_and_sequential_reuse() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let parse = |source: &str| {
            skull_parser::parse_module_clean(source, Ustr::from("<inline-census-incremental-test>"))
                .expect("parse")
        };
        let v1 = CENSUS_SRC;
        let v2 = CENSUS_SRC.replace("return flag", "return false");
        let mut modules = vec![parse(v1)];
        let ws1 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v1");
        let mid1 = *ws1
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v1");
        let (full1, mut cache) = emit_full(&ws1, mid1).expect("full v1");
        assert!(
            full1
                .attribution
                .inline
                .population
                .canonical_clone_derivations
                > 0,
            "fresh build did not use canonical consumers: {:?}",
            full1.attribution.inline.population
        );

        let mut modules = vec![parse(&v2)];
        let ws2 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v2");
        let mid2 = *ws2
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v2");
        let straight = ref_for(&ws2, "straight");
        let reinferred = vec![(straight.module, BodyOwner::Function(straight.func))];
        let incremental =
            emit_incremental(&ws2, &mut cache, &reinferred).expect("incremental canonical reuse");
        let fresh = emit_full(&ws2, mid2).expect("fresh v2").0;
        assert_eq!(
            incremental.lua, fresh.lua,
            "incremental output drifted from fresh output"
        );
        assert!(
            incremental.attribution.inline.candidate_setup_reused > 0
                && incremental
                    .attribution
                    .inline
                    .population
                    .ordinary_canonical_miss_keys
                    == 0
                && incremental
                    .attribution
                    .inline
                    .population
                    .specialized_canonical_miss_keys
                    == 0,
            "incremental build did not preserve the canonical contract: {:?}",
            incremental.attribution.inline.population
        );

        let v3 = v2.replace("return 2", "return 3");
        let mut modules = vec![parse(&v3)];
        let ws3 = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic v3");
        let mid3 = *ws3
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module v3");
        let general = ref_for(&ws3, "general");
        let sequential = emit_incremental(
            &ws3,
            &mut cache,
            &[(general.module, BodyOwner::Function(general.func))],
        )
        .expect("sequential canonical reuse");
        let fresh3 = emit_full(&ws3, mid3).expect("fresh v3").0;
        assert_eq!(
            sequential.lua, fresh3.lua,
            "sequential incremental output drifted"
        );
        assert_eq!(
            sequential
                .attribution
                .inline
                .population
                .ordinary_canonical_miss_keys,
            0,
            "sequential incremental path lost the canonical contract"
        );

        let mut modules = vec![parse(v1)];
        let ws_overlay_base = bone_semantic::semantic_passes::semantic_pass(&mut modules)
            .expect("semantic overlay base");
        let mid_overlay = *ws_overlay_base
            .module_ids
            .get(&Ustr::from("<inline-census-incremental-test>"))
            .expect("module overlay base");
        let (_full_overlay, mut overlay_cache) =
            emit_full(&ws_overlay_base, mid_overlay).expect("full overlay base");
        let overlay =
            emit_overlay(&ws2, &mut overlay_cache, &reinferred).expect("overlay canonical reuse");
        assert!(
            !overlay.lua.is_empty(),
            "overlay must emit the changed route"
        );
        let table = overlay_cache
            .inline_cache
            .candidate_table()
            .expect("overlay candidate table");
        assert_eq!(
            table.stats().profiles.len(),
            table.stats().keys,
            "overlay publication must profile every published key"
        );
        assert!(
            table.stats().reused > 0,
            "overlay must reuse unaffected canonical profiles"
        );
    }

    #[test]
    fn straight_cross_block_results_use_a_targeted_binding() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = cross_block_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-cross-block-test>"))
            .expect("cross-block test module");
        let (emit, _) = emit_full(&ws, mid).expect("cross-block emission");
        assert!(
            !emit.lua.contains("identity("),
            "the straight callee must still inline in the cross-block fixture"
        );
        assert_eq!(
            emit.attribution.inline.census.substitution_invocations, 0,
            "straight result binding must not scan the caller arena: {:?}",
            emit.attribution.inline.census
        );
    }

    #[test]
    fn body_attribution_reports_transform_populations() {
        let _inline = InlineOn::on();
        let bounded = bounded_workspace();
        let bounded_mid = *bounded
            .module_ids
            .get(&Ustr::from("<bound-plan-test>"))
            .expect("bounded test module");
        let (bounded_emit, _) = emit_full(&bounded, bounded_mid).expect("bounded emission");
        assert!(
            bounded_emit.attribution.counted_loop.bodies_with_work > 0,
            "counted-loop recovery did not report a rewritten body: {:?}",
            bounded_emit.attribution
        );
        assert!(
            bounded_emit.attribution.lower.abi_param_shared_views > 0,
            "lowerer did not exercise the shared ABI parameter view: {:?}",
            bounded_emit.attribution
        );
        assert!(
            bounded_emit.attribution.inline.bounded_splices > 0,
            "bounded fixture did not reach the bounded inline route: {:?}",
            bounded_emit.attribution.inline
        );

        let closure = closure_workspace();
        let closure_mid = *closure
            .module_ids
            .get(&Ustr::from("<closure-test>"))
            .expect("closure test module");
        let (closure_emit, _) = emit_full(&closure, closure_mid).expect("closure emission");
        assert!(
            closure_emit.attribution.closure.bodies_with_work > 0,
            "closure conversion did not report a hoisted body: {:?}",
            closure_emit.attribution
        );
        assert!(
            closure_emit.attribution.inline.hoisted_splices > 0,
            "the closure fixture did not reach the hoisted inline route: {:?}",
            closure_emit.attribution
        );

        let specialized = specialized_workspace();
        let specialized_mid = *specialized
            .module_ids
            .get(&Ustr::from("<specialized-test>"))
            .expect("specialized test module");
        let (specialized_emit, _) =
            emit_full(&specialized, specialized_mid).expect("specialized emission");
        assert!(
            specialized_emit.attribution.inline.specialized_splices > 0,
            "the specialized fixture did not reach specialized inline splicing: {:?}",
            specialized_emit.attribution
        );
    }

    #[test]
    fn body_attribution_includes_explicit_nested_lowering() {
        const NESTED_SRC: &str = r#"
fn outer(x: int) -> int {
    fn inner(y: int) -> int { return y }
    return inner(x)
}
fn main() { let result = outer(1) }
"#;
        let mut modules = vec![
            skull_parser::parse_module_clean(NESTED_SRC, Ustr::from("<nested-lower-test>"))
                .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = ws
            .modules
            .iter()
            .find(|m| m.id != ModuleId::builtin())
            .expect("test module")
            .id;
        let (emit, _) = emit_full(&ws, mid).expect("nested emission");
        assert!(
            emit.attribution.lower.mir_instruction_allocations
                > emit.attribution.lower.instructions_visited,
            "nested lowering was omitted from the arena census: {:?}",
            emit.attribution
        );
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn setup_lanes_use_deterministic_overlap_handshake() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let (emit, cache) = FullEmit::program()
            .mutation(BodyMutation::SetupLaneSchedule(
                SetupLaneSchedule::Handshake,
            ))
            .run(&ws, mid)
            .expect("setup lane emission");
        let lanes = emit.setup_lanes;
        assert!(
            lanes.cpu_population > 0 && lanes.shader_population > 0 && lanes.abi_population > 0,
            "each setup lane must have a real population: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_entered, 2,
            "CPU and shader lanes must both reach the handshake: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_released, 2,
            "both handshake lanes must be released: {lanes:?}"
        );
        assert!(
            lanes.setup_handshake_released_after_both,
            "handshake must release only after both lanes enter: {lanes:?}"
        );
        assert!(
            lanes.mono_intern_new_during_join > 0,
            "the overlap fixture must allocate new shared identity: {lanes:?}"
        );
        assert!(!cache.lua_view().publisher_lanes.is_empty());
        assert!(!cache.shader_view.roots.is_empty());
    }

    #[cfg(feature = "sequential")]
    #[test]
    fn setup_lanes_keep_the_sequential_control() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let (emit, _) = emit_full(&ws, mid).expect("setup lane emission");
        let lanes = emit.setup_lanes;
        assert!(
            lanes.cpu_population > 0 && lanes.shader_population > 0 && lanes.abi_population > 0,
            "each setup lane must have a real population: {lanes:?}"
        );
        assert_eq!(
            lanes.setup_handshake_entered, 0,
            "sequential setup used the parallel handshake"
        );
        assert_eq!(
            lanes.setup_handshake_released, 0,
            "sequential setup released a handshake"
        );
        assert!(!lanes.setup_handshake_released_after_both);
    }

    #[cfg(not(feature = "sequential"))]
    #[test]
    fn setup_lanes_preserve_identity_under_reversed_scheduling() {
        let _inline = InlineOn::on();
        let ws = setup_workspace();
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<setup-lane-test>"))
            .expect("setup test module");
        let emit = |schedule| {
            FullEmit::program()
                .mutation(BodyMutation::SetupLaneSchedule(schedule))
                .run(&ws, mid)
                .expect("setup lane emission")
        };
        let (cpu_first, cpu_cache) = emit(SetupLaneSchedule::CpuFirst);
        let (abi_first, abi_cache) = emit(SetupLaneSchedule::AbiFirst);

        for (name, emit, cache, expected_first) in [
            (
                "CPU-first",
                &cpu_first,
                &cpu_cache,
                SetupLaneKind::CpuArtifact,
            ),
            ("ABI-first", &abi_first, &abi_cache, SetupLaneKind::AbiWarm),
        ] {
            assert!(
                emit.setup_lanes.cpu_population > 0
                    && emit.setup_lanes.shader_population > 0
                    && emit.setup_lanes.abi_population > 0,
                "{name} schedule must exercise every setup lane: {:?}",
                emit.setup_lanes
            );
            assert_eq!(
                emit.setup_lanes.first_started_lane,
                Some(expected_first),
                "{name} schedule did not force the requested first lane: {:?}",
                emit.setup_lanes
            );
            assert!(
                emit.setup_lanes.mono_write_acqs_during_join > 0
                    && emit.setup_lanes.mono_intern_attempts_during_join > 0
                    && emit.setup_lanes.mono_intern_new_during_join > 0,
                "{name} fixture must expose the shared interner write population: {:?}",
                emit.setup_lanes
            );
            assert!(
                cache.lua_view().publisher_lanes.len() > 0,
                "{name} fixture must publish a concrete foreign body: {:?}",
                cache.lua_view().census()
            );
            assert!(
                cache.shader_view.roots.len() > 0,
                "{name} fixture must retain a shader root: {:?}",
                cache.shader_view.roots.len()
            );
        }

        assert_eq!(
            cpu_first.lua, abi_first.lua,
            "reversing setup scheduling changed the Lua artifact"
        );
        assert_eq!(
            cpu_cache.lua_view().publisher_lanes,
            abi_cache.lua_view().publisher_lanes,
            "reversing setup scheduling changed publisher ABI identity"
        );
    }
}
