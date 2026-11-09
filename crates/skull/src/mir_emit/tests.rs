use super::*;

fn module_id(ws: &Workspace, file: &str) -> ModuleId {
    *ws.module_ids.get(&Ustr::from(file)).expect("module id")
}

#[cfg(test)]
mod lua_definition_scope_tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Panics when luajit cannot be spawned: a host without it must report red
    /// for a test that never ran, never green.
    fn luajit_loads(lua: &str) -> (bool, String) {
        let mut child = Command::new("luajit")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("luajit is required for this execution test but could not be spawned");
        if let Err(error) = child.stdin.take().unwrap().write_all(lua.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let output = child.wait_with_output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
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
        let (raw_loaded, raw_error) = luajit_loads(&raw);
        assert!(!raw_loaded, "the unpartitioned control unexpectedly loaded");
        assert!(
            raw_error.contains("more than 65536 constants"),
            "the control failed for the wrong reason: {raw_error}"
        );

        let mut scoped = String::new();
        definitions.append_to(&mut scoped);
        scoped.push_str("assert(generated_0() == 0 and generated_32768() == 32768)\n");
        let (scoped_loaded, scoped_error) = luajit_loads(&scoped);
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
        let mid = ws.source_modules().next().expect("test module").id;
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
        let mid = module_id(&ws1, "<artifact-handle-test>");
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
        let mid = module_id(&ws1, "<overlay-binding-test>");
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
        ws.source_modules()
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
        let mid = ws.source_modules().next().expect("test module").id;
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
        let mid = module_id(&ws, "<bound-plan-test>");
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
        let mid = module_id(&ws, "<coroutine-plan-test>");
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
        let mid = module_id(&ws, "<bound-plan-test>");
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
        let mid = ws.source_modules().next().expect("test module").id;
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
        let mid = ws.source_modules().next().expect("test module").id;
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
        let mid = ws.source_modules().next().expect("test module").id;
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
                "setup: concrete plans+lanes",
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
                "join: concrete plans branch",
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
        let mid = module_id(&ws, "<inline-census-test>");
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

    const LOAD_STORE_SRC: &str = r#"
nobox value Pair { left: int, right: int }

fn split(n: int) -> (int, int) { return (n + 1, n * 2) }

fn make(n: int) -> Pair { return Pair { left: n, right: n + 1 } }

fn tagged(n: int) -> (Pair, int) { return (make(n), n) }

@inline
fn bump(n: int) -> int {
    let doubled = n + n
    return doubled
}

fn run(seed: int) -> int {
    let (a, b) = split(seed)
    let p = make(a + b)
    let t = tagged(seed + 1)
    let w = bump(seed * 3)
    return p.left + p.right + t.1 + w
}

fn main() { let result = run(4) }
"#;

    /// Splices accepted for `run`, whose last statement calls an @inline
    /// callee either through a local (`let x = e; wide(x)`) or directly
    /// (`wide(e)`), after `filler` dead locals that push the caller toward the
    /// inliner's cell budget.
    fn budget_shape_splices(filler: usize, through_local: bool) -> usize {
        let mut src = String::from(
            "@inline\nfn wide(n: int) -> int {\n    let a = n + 1\n    let b = a + n\n    return a + b\n}\n\nfn run(seed: int) -> int {\n",
        );
        for i in 0..filler {
            src.push_str(&format!("    let f{i} = seed + {i}\n"));
        }
        if through_local {
            src.push_str("    let x = seed * 2\n    return wide(x)\n}\n");
        } else {
            src.push_str("    return wide(seed * 2)\n}\n");
        }
        src.push_str("\nfn main() { let result = run(4) }\n");
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(&src, Ustr::from("<budget-shape>")).expect("parse"),
        );
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = module_id(&ws, "<budget-shape>");
        let (emit, _) = emit_full(&ws, mid).expect("intact emission");
        emit.attribution.inline.ordinary_splices
    }

    #[test]
    fn cell_budget_admission_does_not_depend_on_argument_binding_shape() {
        let _inline = InlineOn::on();
        // Binding the argument to a local adds a caller cell, and coalescing the
        // parameter onto it removes a template cell: the spliced total is the
        // same, so the budget must admit both shapes alike at every size.
        let mut seen = std::collections::BTreeSet::new();
        for filler in 385..=400 {
            let local = budget_shape_splices(filler, true);
            let direct = budget_shape_splices(filler, false);
            assert_eq!(
                local, direct,
                "budget admission differs by binding shape at {filler} filler locals"
            );
            seen.insert(local);
        }
        assert!(
            seen.len() > 1,
            "the window must straddle the cell budget: {seen:?}"
        );
    }

    #[test]
    fn cleanup_load_store_census_attributes_unlinked_operations_to_producers() {
        use skull_mir::mir::CellProducer;
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let mut modules = STDLIB.clone();
        modules.push(
            skull_parser::parse_module_clean(LOAD_STORE_SRC, Ustr::from("<load-store-census>"))
                .expect("parse"),
        );
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = module_id(&ws, "<load-store-census>");
        let (emit, _) = emit_full(&ws, mid).expect("intact emission");
        let census = &emit.attribution.inline.census.load_store;
        let row = |producer: CellProducer| census.rows[producer as usize];
        for producer in CellProducer::ALL {
            let r = row(producer);
            assert!(
                r.loads_unlinked <= r.loads_before
                    && r.stores_unlinked <= r.stores_before
                    && r.spliced_loads_unlinked <= r.loads_unlinked
                    && r.spliced_stores_unlinked <= r.stores_unlinked,
                "{} row cannot unlink more than it saw: {r:?}",
                producer.label()
            );
        }
        let unlinked = |producer: CellProducer| {
            let r = row(producer);
            r.loads_unlinked + r.stores_unlinked
        };
        assert!(
            unlinked(CellProducer::Destructure) > 0,
            "`let (a, b)` leaf cells must be attributed to destructuring: {census:?}"
        );
        assert!(
            unlinked(CellProducer::AggregateLeaf) > 0,
            "a value local's leaf cells must be attributed to aggregate leaves: {census:?}"
        );
        assert!(
            row(CellProducer::TupleSlot).stores_before > 0,
            "a splatting tuple local must be attributed to tuple slots: {census:?}"
        );
        let forwarded = row(CellProducer::InlineParam);
        assert!(
            forwarded.stores_unlinked > 0
                && forwarded.spliced_stores_unlinked == forwarded.stores_unlinked
                && forwarded.spliced_loads_unlinked == forwarded.loads_unlinked,
            "only spliced code touches a non-coalesced inline parameter: {census:?}"
        );
        for producer in [
            CellProducer::AggregateLeaf,
            CellProducer::TupleSlot,
            CellProducer::Destructure,
        ] {
            let r = row(producer);
            assert!(
                r.spliced_loads_unlinked + r.spliced_stores_unlinked == 0,
                "caller-lowered {} cells are not inline forwarding: {r:?}",
                producer.label()
            );
        }
    }

    #[test]
    fn inline_population_census_proves_each_lowering_route_and_conserves_keys() {
        let _inline = InlineOn::on();
        let _census = InlineCensusOn::on();
        let ws = census_workspace();
        let mid = module_id(&ws, "<inline-census-test>");
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
        let specialized_mid = module_id(&specialized, "<specialized-test>");
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
        let mid1 = module_id(&ws1, "<inline-census-incremental-test>");
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
        let mid2 = module_id(&ws2, "<inline-census-incremental-test>");
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
        let mid3 = module_id(&ws3, "<inline-census-incremental-test>");
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
        let mid_overlay = module_id(&ws_overlay_base, "<inline-census-incremental-test>");
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
        let mid = module_id(&ws, "<inline-cross-block-test>");
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
        let bounded_mid = module_id(&bounded, "<bound-plan-test>");
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
        let closure_mid = module_id(&closure, "<closure-test>");
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
        let specialized_mid = module_id(&specialized, "<specialized-test>");
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
        let mid = ws.source_modules().next().expect("test module").id;
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
        let mid = module_id(&ws, "<setup-lane-test>");
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
        let mid = module_id(&ws, "<setup-lane-test>");
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
        let mid = module_id(&ws, "<setup-lane-test>");
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

/// Adversarial schedules for the specialization queue
/// (`view_driven_specialized_chunks`): forced completion orders, duplicate
/// demand, and conflicting or failing demand. The jobs run on the real
/// scheduler; `SpecScheduleHook` only holds finished jobs back and records
/// what was installed.
#[cfg(all(test, not(feature = "sequential")))]
mod spec_schedule_tests {
    use super::instances::{SPEC_SCHEDULE_HOOK, SpecScheduleHook};
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;
    use ustr::Ustr;

    // `mid<int>` is demanded by `a` and `c`, `leaf<int>` by `a` and again by
    // `mid<int>` once that is built, and `outer<bool>` reaches `leaf<bool>`
    // only through `mid<bool>`. Each `pick{i}` adds its own `count`
    // specialization, as in
    // `instance_owned_emission::specializations_emit_in_a_stable_order`.
    const SRC: &str = r#"
inline(lua) fn tag<V>(value: V) -> int = "return 1"
fn count<V>(items: [V]) -> int {
    let n = 0
    for x in items { n += tag(x) }
    return n
}
struct Item0 { v: int }
struct Item1 { v: int }
struct Item2 { v: int }
struct Item3 { v: int }
struct Item4 { v: int }
struct Item5 { v: int }
fn pick0() -> int { return count([Item0 { v: 0 }]) }
fn pick1() -> int { return count([Item1 { v: 1 }]) }
fn pick2() -> int { return count([Item2 { v: 2 }]) }
fn pick3() -> int { return count([Item3 { v: 3 }]) }
fn pick4() -> int { return count([Item4 { v: 4 }]) }
fn pick5() -> int { return count([Item5 { v: 5 }]) }
struct Cell1 { v: int }
fn leaf<T>(x: T) -> T { return x }
fn mid<T>(x: T) -> T { return leaf(x) }
fn outer<T>(x: T) -> T { return mid(x) }
fn a() -> int { return mid(1) + leaf(2) }
fn b() -> bool { return outer(true) }
fn c() -> int { return mid(3) }
fn d() -> Cell1 { return leaf(Cell1 { v: 4 }) }
fn e() -> Cell1 { return mid(Cell1 { v: 5 }) }
fn main() { let r = a() + c() }
"#;

    const MODULE: &str = "<spec-schedule-test>";
    const TURN_TIMEOUT: Duration = Duration::from_secs(10);

    /// Which queued jobs to hold back, by a substring of their symbol, in the
    /// order they are then released.
    type Held = &'static [&'static str];

    #[derive(Default)]
    struct Log {
        scheduled: Vec<String>,
        completed: Vec<String>,
        installed: Vec<(String, bool)>,
        /// Jobs in flight (scheduled, not completed) when each held job was
        /// released: the held jobs still waiting, and nothing else.
        releases: Vec<(String, usize, usize)>,
        timed_out: Vec<String>,
    }

    /// Holds every job matching `held` until no other queued job is in
    /// flight, then releases the held ones in `held`'s order. A held job's
    /// own demand is queued only once it completes, so every job that can
    /// run before it does, and the queue then drains the rest.
    struct Forced {
        held: Held,
        log: Mutex<Log>,
        waiting: Mutex<Vec<String>>,
        turn: Condvar,
    }

    impl Forced {
        fn new(held: Held) -> Arc<Self> {
            Arc::new(Self {
                held,
                log: Mutex::default(),
                waiting: Mutex::new(Vec::new()),
                turn: Condvar::new(),
            })
        }

        fn rank(&self, name: &str) -> Option<usize> {
            self.held.iter().position(|pat| name.contains(pat))
        }

        fn in_flight(log: &Log) -> usize {
            log.scheduled.len() - log.completed.len()
        }
    }

    impl SpecScheduleHook for Forced {
        fn scheduled(&self, name: &str) {
            self.log.lock().unwrap().scheduled.push(name.to_string());
            self.turn.notify_all();
        }

        fn finished(&self, name: &str) {
            let mut log = self.log.lock().unwrap();
            if let Some(rank) = self.rank(name) {
                self.waiting.lock().unwrap().push(name.to_string());
                // The other held jobs wait on the in-flight count, which this
                // arrival changes.
                self.turn.notify_all();
                loop {
                    let waiting = self.waiting.lock().unwrap().clone();
                    let first = waiting.iter().min_by_key(|held| self.rank(held)).cloned();
                    let my_turn =
                        first.as_deref() == Some(name) && Self::in_flight(&log) == waiting.len();
                    if my_turn {
                        let in_flight = Self::in_flight(&log);
                        log.releases
                            .push((name.to_string(), in_flight, waiting.len()));
                        self.waiting.lock().unwrap().retain(|held| held != name);
                        break;
                    }
                    let (next, wait) = self.turn.wait_timeout(log, TURN_TIMEOUT).unwrap();
                    log = next;
                    if wait.timed_out() {
                        log.timed_out.push(format!("{name} (held #{rank})"));
                        self.waiting.lock().unwrap().retain(|held| held != name);
                        break;
                    }
                }
            }
            log.completed.push(name.to_string());
            self.turn.notify_all();
        }

        fn installed(&self, name: &str, ok: bool) {
            self.log
                .lock()
                .unwrap()
                .installed
                .push((name.to_string(), ok));
        }
    }

    fn workspace() -> Workspace {
        let mut modules: Vec<_> = rock_stdlib::stdlib_files()
            .iter()
            .map(|file| {
                let id = Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str());
                skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(skull_parser::parse_module_clean(SRC, Ustr::from(MODULE)).expect("parse"));
        bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
    }

    /// Enough workers that every held job keeps a thread while the rest of
    /// the queue drains.
    fn pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(32)
            .build()
            .expect("schedule test pool")
    }

    fn with_hook<R: Send>(hook: Option<Arc<Forced>>, f: impl FnOnce() -> R + Send) -> R {
        pool().install(|| {
            SPEC_SCHEDULE_HOOK.with(|slot| {
                *slot.borrow_mut() = hook.map(|hook| hook as Arc<dyn SpecScheduleHook>)
            });
            let result = f();
            SPEC_SCHEDULE_HOOK.with(|slot| *slot.borrow_mut() = None);
            result
        })
    }

    fn emit(ws: &Workspace, hook: Option<Arc<Forced>>) -> (MirEmit, SkullEmitCache) {
        let mid = module_id(ws, MODULE);
        with_hook(hook, || FullEmit::program().run(ws, mid)).expect("schedule fixture emits")
    }

    /// Every held job was released last among the jobs then in flight, in
    /// `held`'s order, and nothing timed out.
    fn assert_forced(hook: &Forced, label: &str) {
        let log = hook.log.lock().unwrap();
        assert!(
            log.timed_out.is_empty(),
            "{label}: jobs never got their turn: {:?}",
            log.timed_out
        );
        let held: Vec<&String> = log
            .scheduled
            .iter()
            .filter(|name| hook.rank(name).is_some())
            .collect();
        assert_eq!(
            log.releases.len(),
            held.len(),
            "{label}: not every held job was released: {:?}",
            log.releases
        );
        for (name, in_flight, waiting) in &log.releases {
            assert_eq!(
                in_flight, waiting,
                "{label}: `{name}` was released while another job was in flight"
            );
        }
        let released: Vec<Option<usize>> = log
            .releases
            .iter()
            .map(|(name, _, _)| hook.rank(name))
            .collect();
        let mut expected = released.clone();
        expected.sort();
        assert_eq!(
            released, expected,
            "{label}: held jobs released out of order"
        );
    }

    const SCHEDULES: [(&str, Held); 4] = [
        ("hold mid<int>", &["mid_T_spec_p_int"]),
        (
            "hold count<Item0> then leaf<int>",
            &["count_arr_V_spec_a_n5_Item0", "leaf_T_spec_p_int"],
        ),
        (
            "hold leaf<int> then count<Item0>",
            &["leaf_T_spec_p_int", "count_arr_V_spec_a_n5_Item0"],
        ),
        ("hold every count", &["count_arr_V_spec"]),
    ];

    /// `held` patterns are symbol substrings; make sure each one names a job
    /// that really was queued, so a renamed symbol fails loudly.
    fn assert_held_exist(hook: &Forced, label: &str) {
        let log = hook.log.lock().unwrap();
        for pat in hook.held {
            assert!(
                log.scheduled.iter().any(|name| name.contains(pat)),
                "{label}: no queued job matches `{pat}`: {:?}",
                log.scheduled
            );
        }
    }

    #[test]
    fn forced_completion_orders_install_and_emit_identically() {
        let ws = workspace();
        let (natural, _) = emit(&ws, None);
        let mut reference: Option<(BTreeSet<String>, Vec<(String, bool)>)> = None;
        for (label, held) in SCHEDULES {
            let hook = Forced::new(held);
            let (emit, _) = emit(&ws, Some(Arc::clone(&hook)));
            assert_held_exist(&hook, label);
            assert_forced(&hook, label);
            assert_eq!(
                emit.lua, natural.lua,
                "{label}: forced completion order changed the emitted program"
            );
            let log = hook.log.lock().unwrap();
            // Duplicate demand: two chunks demand `mid<int>` and `leaf<int>` is
            // demanded again once `mid<int>` is built, yet each instance is
            // scheduled and installed once.
            let scheduled: BTreeSet<String> = log.scheduled.iter().cloned().collect();
            assert_eq!(
                scheduled.len(),
                log.scheduled.len(),
                "{label}: an instance was built twice: {:?}",
                log.scheduled
            );
            assert!(
                scheduled.len() >= 8,
                "{label}: the fixture must queue many jobs: {scheduled:?}"
            );
            let observed = (scheduled, log.installed.clone());
            match &reference {
                None => reference = Some(observed),
                Some(reference) => assert_eq!(
                    &observed, reference,
                    "{label}: the jobs or the installation order depend on completion order"
                ),
            }
        }

        let (_, installed) = reference.expect("schedules ran");
        let installed_names: BTreeSet<&String> = installed.iter().map(|(name, _)| name).collect();
        assert_eq!(installed.len(), installed_names.len(), "{installed:?}");
        assert!(installed.iter().all(|(_, ok)| *ok), "{installed:?}");
    }

    fn spec_named(cache: &SkullEmitCache, ws: &Workspace, name: &str, arg: &str) -> Instance {
        cache
            .graph
            .instances
            .iter()
            .find(|inst| {
                let f = &ws.module(inst.func.module).functions[inst.func.func];
                f.name.as_str() == name
                    && !inst.args.is_empty()
                    && inst.symbol(ws, cache.scx.interner()).contains(arg)
            })
            .cloned()
            .unwrap_or_else(|| panic!("no `{name}` instance at `{arg}`"))
    }

    /// A conflicting demand (two bound-tree answers for one instance) and a
    /// job whose build fails share the queue with successful jobs. The
    /// rejections, the surviving chunks and the installation order must not
    /// depend on which job finishes first.
    #[test]
    fn conflicting_and_failing_demand_report_identically_under_forced_orders() {
        let ws = workspace();
        let (_, mut cache) = emit(&ws, None);
        let clashing = spec_named(&cache, &ws, "mid", "bool");
        let failing = spec_named(&cache, &ws, "leaf", "Cell1");
        let owner = cache
            .graph
            .instances
            .iter()
            .find(|inst| inst.args.is_empty())
            .cloned()
            .expect("a base instance");
        let mut dissent = BTreeMap::new();
        dissent.insert(
            crate::instance_graph::Owner::Instance(clashing.clone()),
            crate::instance_graph::InstanceBoundTree::default(),
        );
        cache.graph.index.trees.insert(
            clashing.clone(),
            crate::instance_graph::Agreed {
                owners: BTreeSet::from([crate::instance_graph::Owner::Instance(owner)]),
                value: crate::instance_graph::InstanceBoundTree::default(),
                dissent,
                total: 2,
            },
        );
        std::sync::Arc::make_mut(&mut cache.concrete.plans).remove(&failing);
        // A retained cache already holds every specialized chunk; drop them so
        // the queue has to build each one again.
        cache.chunks.retain(|inst, _| inst.args.is_empty());

        let run = |held: Held| {
            let hook = Forced::new(held);
            let (built, errs) = with_hook(Some(Arc::clone(&hook)), || {
                view_driven_specialized_chunks(&ws, &cache.cg, &cache.scx, &cache, &[], None)
            });
            let mut chunks: Vec<(String, String)> = built
                .iter()
                .map(|(inst, chunk)| (inst.symbol(&ws, cache.scx.interner()), chunk.lua.clone()))
                .collect();
            chunks.sort();
            let errs: Vec<(String, String)> = errs
                .into_iter()
                .map(|(name, err)| (name, err.message))
                .collect();
            let installed = hook.log.lock().unwrap().installed.clone();
            (hook, chunks, errs, installed)
        };

        let (first, chunks, errs, installed) = run(&[]);
        assert_forced(&first, "natural");
        let clash_name = clashing.symbol(&ws, cache.scx.interner());
        let failing_name = failing.symbol(&ws, cache.scx.interner());
        // `mid<Cell1>` lowers a call to the plan-less `leaf<Cell1>`, so it
        // fails alongside it.
        let caller_name = spec_named(&cache, &ws, "mid", "Cell1").symbol(&ws, cache.scx.interner());
        let mut rejected: Vec<&String> = errs.iter().map(|(name, _)| name).collect();
        rejected.sort();
        let mut expected = vec![&clash_name, &failing_name, &caller_name];
        expected.sort();
        assert_eq!(rejected, expected, "{errs:?}");
        assert!(
            errs.iter().any(|(name, message)| *name == clash_name
                && message.contains("different `Iterator` bound resolutions")),
            "the conflicting demand was not rejected: {errs:?}"
        );
        assert!(
            errs.iter().any(
                |(name, message)| *name == failing_name && message.contains("no concrete plan")
            ),
            "the failing job was not reported: {errs:?}"
        );
        assert!(
            first.log.lock().unwrap().scheduled.contains(&failing_name),
            "the failing job must be queued alongside successful ones"
        );
        assert!(
            chunks.iter().all(|(name, _)| !expected.contains(&name)),
            "a rejected instance still produced a chunk"
        );
        // Only the clashing instance demands `leaf<bool>`, so its rejection
        // must also keep that instance out.
        assert!(
            !chunks
                .iter()
                .any(|(name, _)| name.starts_with("leaf") && name.contains("bool")),
            "a rejected demand's callees were still built: {chunks:?}"
        );

        for (label, held) in [
            (
                "hold the failing job",
                &["leaf_T_spec_a_n5_Cell1", "count_arr_V_spec"] as Held,
            ),
            (
                "hold mid<Cell1> then count",
                &["mid_T_spec_a_n5_Cell1", "count_arr_V_spec_a_n5_Item0"],
            ),
        ] {
            let (hook, other_chunks, other_errs, other_installed) = run(held);
            assert_held_exist(&hook, label);
            assert_forced(&hook, label);
            assert_eq!(
                other_errs, errs,
                "{label}: diagnostics depend on completion order"
            );
            assert_eq!(
                other_chunks, chunks,
                "{label}: chunks depend on completion order"
            );
            assert_eq!(
                other_installed, installed,
                "{label}: installation order depends on completion order"
            );
        }
    }
}
