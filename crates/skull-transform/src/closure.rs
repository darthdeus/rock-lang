//! Closure conversion as a shared MIR->MIR pass.
//! Replaces the per-backend closure
//! machinery (codegen-lua's `HoistPlan`/`EnvCtx`; the JS T5/T7/T8c native-arrow
//! path) with ONE transform: every `Inst::Lambda` becomes either a bare
//! [`Inst::Global`] (capture-free) or an env-record [`Inst::MakeEnv`] (capturing),
//! and its sub-CFG is split out to a sibling [`MirFunc`] in `nested_fns`.
//!
//! The capture SEMANTICS (by-value vs box, per-iteration loop bindings, the
//! dominance rail) live in `crate::scope` + `non_dominating_by_value_captures`
//! (moved here from codegen-lua at S3), so there is one mechanism and no
//! divergence. What is new is that the analysis result is MATERIALIZED as MIR
//! (`MakeEnv` records, `Field`/`StoreField` env access, box tables) instead of
//! being consumed at render time.
//!
//! Every servable path routes through this pass (S1 JS, S2 ordinary Lua fns, S3
//! coroutine steps + global-init/decorator units), so post-pass codegen sees
//! `MakeEnv`/`Global`, never an `Inst::Lambda`.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::scope::{Ownership, ScopeId, ScopeTree, non_dominating_by_value_captures};
use comfy_la_arena::Idx;
use skull_mir::cfg::reachable_blocks_from;
use skull_mir::code_identity::{
    CaptureField, CaptureIdentity, CaptureMode, CaptureSchema, CodeCompatibility, CodeIdentity,
    FrameSchema, LambdaSiteId, LambdaTopologyFingerprint,
};
use skull_mir::mir::*;
use ustr::Ustr;

fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// Environment slots are local to the capture plan, so copies of one lambda
/// site have the same body even when injected into different MIR arena slots.
fn field_of(plan: &ClosureEnvPlan, c: Idx<Cell>) -> Ustr {
    let slot = plan
        .captures()
        .binary_search_by_key(&raw(c), |cell| raw(*cell))
        .expect("environment field belongs to its capture plan");
    Ustr::from(format!("c{slot}").as_str())
}

// Only compiler-owned unwind closures are eligible: their identity and return
// value never escape the cleanup stack. A mutable capture must keep its box so
// reassignment after registration is still observed when unwinding.
fn forwarded_defer_capture(
    mf: &MirFunc,
    scope: &crate::scope::Scope,
    captures: &[Idx<Cell>],
    boxed: &HashSet<Idx<Cell>>,
) -> Option<Idx<Cell>> {
    let [capture] = captures else { return None };
    if !scope.params.is_empty() || boxed.contains(capture) {
        return None;
    }
    let mut block = scope.entry;
    let mut visited = HashSet::default();
    let mut loaded = None;
    let mut called = false;
    loop {
        if !visited.insert(block) {
            return None;
        }
        for &id in &mf.blocks[block].insts {
            match &mf.insts[id] {
                Inst::Load(cell) if cell == capture && loaded.is_none() && !called => {
                    loaded = Some(id)
                }
                Inst::CallValue { callee, args }
                    if Some(*callee) == loaded && args.is_empty() && !called =>
                {
                    called = true
                }
                Inst::Const(Lit::Nil) => {}
                _ => return None,
            }
        }
        match &mf.blocks[block].term {
            Term::Goto(next) => block = *next,
            Term::Return(values) if values.is_empty() && called => return Some(*capture),
            _ => return None,
        }
    }
}

/// The dense frame-field name a coroutine frame cell renders under (`c<slot>`) --
/// keyed on the DENSE slot, NOT the raw id (the P2-finding fix). Must match
/// `FrameCtx`'s `write_cellname` (`s.c<slot>` in the step) and the constructor's
/// seeded field names.
fn frame_field(slot: u32) -> Ustr {
    Ustr::from(format!("c{slot}").as_str())
}

/// What the shared pass needs of a coroutine step's frame (a subset of codegen's
/// `FrameCtx`): the state-table cell `s`, the frame-field cell set, and the dense
/// slot per frame cell. Threaded in only when converting the STEP `MirFunc`
/// (`emit_coroutine`); `None` for ordinary fns and the JS path (no coroutines).
/// A captured frame cell collapses to a capture of `s` (D2), and a frame-cell
/// access inside a hoisted lambda body materializes `e.c<S>.c<slot>` (D3).
pub struct FrameInfo {
    pub s_cell: Idx<Cell>,
    pub fields: HashSet<Idx<Cell>>,
    pub slots: HashMap<Idx<Cell>, u32>,
}

/// Convert every closure in `mf` to a defunctionalized record + a hoisted sibling
/// fn. `name_base` is the enclosing fn's UNIQUE emitted name (the JS name on this
/// slice; the mangled Lua name at S2): hoisted bodies are `<name_base>__lambda<k>`
/// (`k` = the `Inst::Lambda`'s inst-arena order). Post-pass no `Inst::Lambda`
/// survives (asserted); the enclosing region keeps its blocks (lambda regions go
/// unreachable in it) and APPENDS one `nested_fn` per live lambda (preserving any
/// named nested `fn`s lowering already hoisted there).
///
/// DEVIATION from the plan's `convert_closures(mf)` signature: the unique name is
/// passed IN (the pass is MirFunc-local and has no name-mangling context of its
/// own). The caller supplies a collision-free base -- on JS that is
/// `callee_js_name`, itself overload-resolved -- so `<base>__lambda<k>` is unique
/// by construction.
/// The frame-less entry (ordinary fns + the JS path): no coroutine frame, builds
/// its own scope tree.
pub fn convert_closures(mf: &mut MirFunc, name_base: &str) {
    convert_closures_framed(mf, name_base, None, None);
}

/// Convert closures under an optional coroutine frame (`emit_coroutine` supplies
/// [`FrameInfo`] for the STEP `MirFunc`). With a frame, captured frame cells
/// collapse to a capture of `s` (D2) except provenance-tagged loop bindings no
/// lambda writes, which snapshot by value (D4(b)); a frame-cell access inside a
/// hoisted body materializes `e.c<S>.c<slot>` (D3). Detached hook/defer regions
/// are converted by [`lower_detached_region`] before they reach either backend.
///
/// `precomputed` reuses the coroutine transform's `ScopeTree`+`Ownership` (the
/// SAME basis `frame.fields` was derived from). This is not merely the P2
/// single-build optimization: it is REQUIRED for soundness under a frame -- the
/// transform's tree is built pre-yield-rewrite, and a freshly-rebuilt tree can
/// disagree about a frame cell's owner (a cell the transform classed ROOT-owned
/// -> a frame field can rebuild as lambda-owned), leaving a frame-cell access
/// with no env to reach `s` through. Ordinary fns pass `None` and build their own.
pub fn convert_closures_framed(
    mf: &mut MirFunc,
    name_base: &str,
    frame: Option<&FrameInfo>,
    precomputed: Option<(&ScopeTree, &Ownership)>,
) {
    if !mf
        .insts
        .iter()
        .any(|(_, i)| matches!(i, Inst::Lambda { .. }))
    {
        refresh_owner_compatibility(mf, None, LambdaTopologyFingerprint::default());
        return;
    }

    // --- Analysis: REUSE the Lua path's classification verbatim. The frame
    // partition (D2/D4(b)) is applied to the per-scope capture sets below; the
    // frame-cell body access (D3) is materialized in the block loop. ---
    let owned_tree;
    let owned_own;
    let (tree, own): (&ScopeTree, &Ownership) = match precomputed {
        Some((t, o)) => (t, o),
        None => {
            owned_tree = ScopeTree::build(mf);
            owned_own = owned_tree.analyze(mf);
            (&owned_tree, &owned_own)
        }
    };
    let mut boxed: HashSet<Idx<Cell>> = tree.boxed_cells(own);
    // Frame cells are table-mediated (via `s` or a by-value loop-binding snapshot
    // of a frame field), never boxed -- matches codegen's `b.retain(!frame)`.
    if let Some(f) = frame {
        boxed.retain(|c| !f.fields.contains(c));
    }
    boxed.extend(non_dominating_by_value_captures(
        mf,
        tree,
        own,
        &boxed,
        frame.map(|f| &f.fields),
    ));

    // lambda inst -> its scope.
    let mut lambda_sid: HashMap<Idx<Inst>, ScopeId> = HashMap::default();
    for (sid, s) in tree.scopes.iter().enumerate() {
        if let Some(lid) = s.lambda {
            lambda_sid.insert(lid, sid);
        }
    }
    let seeded_boxes = seeded_box_initializers(mf, tree, own, &boxed, &lambda_sid);
    // Live lambdas in inst-arena order (k). A DCE'd lambda (no scope) has no k
    // consumer beyond the counter -- so a dead one never shifts the live names.
    let mut ordered: Vec<(usize, ScopeId)> = Vec::new();
    {
        let mut k = 0usize;
        for (lid, inst) in mf.insts.iter() {
            let Inst::Lambda { .. } = inst else { continue };
            let this_k = k;
            k += 1;
            if let Some(&sid) = lambda_sid.get(&lid) {
                ordered.push((this_k, sid));
            }
        }
    }

    let topology_sites: Vec<(LambdaSiteId, usize)> = ordered
        .iter()
        .filter_map(|&(_, sid)| {
            let lambda = tree.scopes[sid].lambda?;
            let site = mf
                .inst_spans
                .get(lambda)
                .copied()
                .and_then(LambdaSiteId::from_span)?;
            let Inst::Lambda { params, .. } = &mf.insts[lambda] else {
                return None;
            };
            Some((site, params.len()))
        })
        .collect();
    let topology = LambdaTopologyFingerprint::from_sites(&topology_sites);
    refresh_owner_compatibility(mf, frame, topology);

    // Per live lambda scope: hoisted name, sorted captures, an env param cell
    // (only when it captures anything -- a capture-free body takes no env).
    let mut hoisted_name: HashMap<ScopeId, Ustr> = HashMap::default();
    let mut env_plans: HashMap<ScopeId, ClosureEnvPlan> = HashMap::default();
    // Post-partition capture set per scope, for O(1) membership in the block loop:
    // a member is captured DIRECTLY (env field `e.c<slot>`); a frame cell NOT in the
    // set (collapsed to `s`) reaches via `e.c<S>.c<slot>` instead.
    for &(k, sid) in &ordered {
        hoisted_name.insert(sid, Ustr::from(format!("{name_base}__lambda{k}").as_str()));
        let mut caps: Vec<Idx<Cell>> = own.captures[sid].iter().copied().collect();
        // Frame partition (D2 + D4(b)), ported
        // verbatim from `build_hoist_plan`: a captured FRAME cell either snapshots
        // by value at record creation (a provenance-tagged loop binding no lambda
        // writes -- per-iteration, the non-frame rule) or collapses to a capture of
        // `s` itself (shared through the frame table; the body composes
        // `e.c<S>.c<slot>`). Lambda-owned cells (nested captures) keep the standard
        // classification.
        if let Some(f) = frame {
            let mut via_s = false;
            caps.retain(|c| {
                if !f.fields.contains(c) {
                    return true;
                }
                let lambda_written = own
                    .writing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != crate::scope::ROOT));
                if own.loop_bindings.contains(c) && !lambda_written {
                    true
                } else {
                    via_s = true;
                    false
                }
            });
            // A detached hook/defer has only `s` as a parameter. Rebuilding its
            // scope tree therefore makes a frame field used solely by a nested
            // lambda look lambda-owned; the coroutine transform remains the
            // authority for that cell's ownership.
            via_s |= f.fields.iter().any(|c| own.owner.get(c) == Some(&sid));
            if via_s {
                caps.push(f.s_cell);
            }
        }
        caps.sort_by_key(|c| raw(*c));
        let env_cell = (!caps.is_empty()).then(|| mf.new_cell(Some(Ustr::from("__rock_env"))));
        env_plans.insert(sid, ClosureEnvPlan::new(caps, env_cell));
    }
    for (&lambda, &sid) in &lambda_sid {
        if let Some(plan) = env_plans.get(&sid) {
            mf.closure_env_plans.insert(lambda, plan.clone());
        }
    }

    let mut forwarded_defers = HashMap::default();
    if frame.is_none() {
        for inst in mf.insts.values() {
            let Inst::CallValue { callee, args } = inst else {
                continue;
            };
            if !matches!(&mf.insts[*callee], Inst::Global(name) if name.as_str() == DEFER_PUSH) {
                continue;
            }
            let [lambda] = args.as_slice() else { continue };
            let Some(&sid) = lambda_sid.get(lambda) else {
                continue;
            };
            if let Some(capture) =
                forwarded_defer_capture(mf, &tree.scopes[sid], env_plans[&sid].captures(), &boxed)
            {
                forwarded_defers.insert(*lambda, capture);
            }
        }
    }

    // --- Rewrite every scoped block: captured/boxed cell accesses become env /
    // `.v` projections, and each `Inst::Lambda` creation site becomes its record
    // (or a bare global). Helper insts are spliced immediately before the inst
    // that reads them; the original id is REPURPOSED as the final projection so
    // every existing reference to it resolves to the projected value. ---
    let block_ids: Vec<Idx<Block>> = mf.blocks.iter().map(|(b, _)| b).collect();
    for b in block_ids {
        let Some(s) = tree.scope_of_block(b) else {
            continue;
        }; // dead-lambda block
        let orig = mf.blocks[b].insts.clone();
        let mut new: Vec<Idx<Inst>> = Vec::with_capacity(orig.len());
        for id in orig {
            match mf.insts[id].clone() {
                Inst::Load(c) => {
                    let captured = env_plans.get(&s).is_some_and(|plan| plan.captures_cell(c));
                    let boxed_c = boxed.contains(&c);
                    // A frame cell accessed in a LAMBDA scope but NOT captured
                    // directly (collapsed to `s`): reach it as `e.c<S>.c<slot>`.
                    // Root-scope frame cells stay bare (`FrameCtx` renders them);
                    // a loop-binding snapshot IS captured, so it takes the env path.
                    let frame_via_s = !captured
                        && s != crate::scope::ROOT
                        && frame.is_some_and(|f| f.fields.contains(&c));
                    if !captured && !boxed_c && !frame_via_s {
                        new.push(id); // local by-value: read the cell as-is
                        continue;
                    }
                    // Splice the helper insts (env load, field projection) then
                    // REPURPOSE `id` as the final read so every reference to it
                    // resolves to the projected value.
                    if frame_via_s {
                        let f = frame.unwrap();
                        let sbase = env_frame_base(mf, &env_plans[&s], f.s_cell, &mut new);
                        mf.insts[id] = Inst::Field {
                            base: sbase,
                            field: frame_field(f.slots[&c]),
                        };
                    } else if captured {
                        // A boxed cell reads `.v` on its box table; a by-value
                        // capture is the env field directly.
                        let env = env_plans[&s].env_cell().expect("capture has an env plan");
                        let il = mf.alloc_inst(Inst::Load(env));
                        new.push(il);
                        if boxed_c {
                            let f = mf.alloc_inst(Inst::Field {
                                base: il,
                                field: field_of(&env_plans[&s], c),
                            });
                            new.push(f);
                            mf.insts[id] = Inst::Field {
                                base: f,
                                field: Ustr::from("v"),
                            };
                        } else {
                            mf.insts[id] = Inst::Field {
                                base: il,
                                field: field_of(&env_plans[&s], c),
                            };
                        }
                    } else {
                        // Local boxed: `.v` on a load of the box table.
                        let l = mf.alloc_inst(Inst::Load(c));
                        new.push(l);
                        mf.insts[id] = Inst::Field {
                            base: l,
                            field: Ustr::from("v"),
                        };
                    }
                    new.push(id);
                }
                Inst::Store { cell: c, value } => {
                    if seeded_boxes.get(&c) == Some(&id) {
                        let seed = mf.alloc_inst(Inst::MakeEnv {
                            fn_ref: None,
                            fields: vec![(Ustr::from("v"), value)],
                        });
                        new.push(seed);
                        mf.insts[id] = Inst::Store {
                            cell: c,
                            value: seed,
                        };
                        new.push(id);
                        continue;
                    }
                    let boxed_c = boxed.contains(&c);
                    let captured = env_plans.get(&s).is_some_and(|plan| plan.captures_cell(c));
                    // A lambda WRITING a frame cell collapses to `s` (D4(b)): the
                    // write steers the shared frame field `e.c<S>.c<slot> = ..`.
                    let frame_via_s = !boxed_c
                        && !captured
                        && s != crate::scope::ROOT
                        && frame.is_some_and(|f| f.fields.contains(&c));
                    if !boxed_c && !frame_via_s {
                        new.push(id); // local plain store (root frame store -> FrameCtx)
                        continue;
                    }
                    let (base, field) = if frame_via_s {
                        let f = frame.unwrap();
                        let sbase = env_frame_base(mf, &env_plans[&s], f.s_cell, &mut new);
                        (sbase, frame_field(f.slots[&c]))
                    } else if captured {
                        let env = env_plans[&s].env_cell().expect("capture has an env plan");
                        let il = mf.alloc_inst(Inst::Load(env));
                        new.push(il);
                        let f = mf.alloc_inst(Inst::Field {
                            base: il,
                            field: field_of(&env_plans[&s], c),
                        });
                        new.push(f);
                        (f, Ustr::from("v"))
                    } else {
                        let l = mf.alloc_inst(Inst::Load(c));
                        new.push(l);
                        (l, Ustr::from("v"))
                    };
                    mf.insts[id] = Inst::StoreField { base, field, value };
                    new.push(id);
                }
                Inst::Lambda { .. } => {
                    let child = lambda_sid[&id];
                    let caps = env_plans[&child].captures();
                    if let Some(&capture) = forwarded_defers.get(&id) {
                        let value = slot_in_scope(mf, own, &env_plans, s, capture, &mut new);
                        mf.insts[id] = mf.insts[value].clone();
                        new.push(id);
                        continue;
                    }
                    if caps.is_empty() {
                        let hoisted = hoisted_name[&child];
                        // A capture-free lambda becomes a top-level fn of THIS
                        // body's own output, so the name it is now called
                        // through is not a callee declaration's symbol.
                        if !mf.hoisted_fn_globals.contains(&hoisted) {
                            mf.hoisted_fn_globals.push(hoisted);
                        }
                        mf.insts[id] = Inst::Global(hoisted);
                        new.push(id);
                        continue;
                    }
                    // Record field values: each capture's SLOT (box table / value)
                    // read FROM this scope -- a transitive capture threads through
                    // this scope's own env.
                    let mut fields: Vec<(Ustr, Idx<Inst>)> = Vec::with_capacity(caps.len());
                    for &cc in caps {
                        let slot = slot_in_scope(mf, own, &env_plans, s, cc, &mut new);
                        fields.push((field_of(&env_plans[&child], cc), slot));
                    }
                    mf.insts[id] = Inst::MakeEnv {
                        fn_ref: Some(hoisted_name[&child]),
                        fields,
                    };
                    new.push(id);
                }
                _ => new.push(id),
            }
        }
        mf.blocks[b].insts = new;
    }

    // --- Unseeded locals still need an entry box. Parameters seed from their
    // incoming argument before rebinding; proven local initializers seed in place. ---
    let mut entry_prepend: HashMap<Idx<Block>, Vec<Idx<Inst>>> = HashMap::default();
    // Sorted so the created boxes / declarations are traversal-independent.
    let mut boxed_sorted: Vec<Idx<Cell>> = boxed.iter().copied().collect();
    boxed_sorted.sort_by_key(|c| raw(*c));
    for c in boxed_sorted {
        if seeded_boxes.contains_key(&c) {
            continue;
        }
        let owner = own.owner[&c];
        let entry = tree.scopes[owner].entry;
        let is_param = tree.scopes[owner].params.contains(&c);
        let dest = entry_prepend.entry(entry).or_default();
        if is_param {
            let seed = mf.alloc_inst(Inst::Load(c));
            let boxv = mf.alloc_inst(Inst::MakeEnv {
                fn_ref: None,
                fields: vec![(Ustr::from("v"), seed)],
            });
            let st = mf.alloc_inst(Inst::Store {
                cell: c,
                value: boxv,
            });
            dest.extend([seed, boxv, st]);
        } else {
            let boxv = mf.alloc_inst(Inst::MakeEnv {
                fn_ref: None,
                fields: Vec::new(),
            });
            let st = mf.alloc_inst(Inst::Store {
                cell: c,
                value: boxv,
            });
            dest.extend([boxv, st]);
        }
    }
    for (entry, mut pre) in entry_prepend {
        pre.extend(mf.blocks[entry].insts.iter().copied());
        mf.blocks[entry].insts = pre;
    }

    // --- Neutralize any residual (dead, scope-less) `Inst::Lambda`; a lambda in a
    // REACHABLE block was converted above, so a survivor there is a pass bug. ---
    let live: HashSet<Idx<Block>> = reachable_blocks_from(mf, mf.entry).into_iter().collect();
    for b in &live {
        for &iid in &mf.blocks[*b].insts {
            assert!(
                !matches!(mf.insts[iid], Inst::Lambda { .. }),
                "convert_closures left an Inst::Lambda reachable from entry (pass bug)"
            );
        }
    }
    // Detached regions are lowered independently by `lower_detached_region`.
    // The step arena therefore has no emitted residual closure producers; only
    // the frame-less path also nils any dead ordinary-function residue.
    if frame.is_none() {
        for (_, inst) in mf.insts.iter_mut() {
            if matches!(inst, Inst::Lambda { .. }) {
                *inst = Inst::Const(Lit::Nil);
            }
        }
    }

    // --- Split each live lambda body into a sibling MirFunc. A full arena clone
    // (already fully rewritten above) keeps every `Idx` valid with zero remapping;
    // the clone emits only its own region (entry = the lambda entry) -- the
    // enclosing + sibling regions go unreachable, exactly as they do in `mf`. ---
    let mut nested: Vec<MirFunc> = Vec::with_capacity(ordered.len());
    for &(_k, sid) in &ordered {
        let lambda = tree.scopes[sid]
            .lambda
            .expect("ordered lambda scope has a creator");
        if forwarded_defers.contains_key(&lambda) {
            continue;
        }
        let mut params: Vec<Idx<Cell>> = Vec::new();
        if let Some(e) = env_plans.get(&sid).and_then(ClosureEnvPlan::env_cell) {
            params.push(e);
        }
        params.extend(tree.scopes[sid].params.iter().copied());
        let code_identity = mf
            .code_identity
            .as_ref()
            .and_then(CodeIdentity::owner)
            .zip(
                mf.inst_spans
                    .get(lambda)
                    .copied()
                    .and_then(LambdaSiteId::from_span),
            )
            .map(|(enclosing, site)| CodeIdentity::Lambda { enclosing, site });
        let capture = capture_schema(mf, env_plans[&sid].captures(), &boxed, frame);
        let compatibility = Some(CodeCompatibility {
            call_abi: mf
                .lambda_abi_fingerprints
                .get(&lambda)
                .cloned()
                .unwrap_or_else(|| "missing-lambda-abi".to_string()),
            capture,
            frame: frame_schema(frame),
            lambda_topology: topology,
        });
        if let Some(identity) = code_identity.clone() {
            mf.code_value_identities.insert(lambda, identity.clone());
            mf.nested_fn_identities.insert(hoisted_name[&sid], identity);
        }
        nested.push(MirFunc {
            name: hoisted_name[&sid],
            code_identity,
            code_abi_fingerprint: mf.lambda_abi_fingerprints.get(&lambda).cloned(),
            lambda_abi_fingerprints: mf.lambda_abi_fingerprints.clone(),
            lambda_abi_representations: mf.lambda_abi_representations.clone(),
            code_value_identities: mf.code_value_identities.clone(),
            nested_fn_identities: mf.nested_fn_identities.clone(),
            code_compatibility: compatibility,
            params,
            param_default_guards: mf.param_default_guards.clone(),
            param_default_materialized: mf.param_default_materialized.clone(),
            param_default_values: mf.param_default_values.clone(),
            lower_timing_enabled: mf.lower_timing_enabled,
            lower_attribution: mf.lower_attribution.clone(),
            abi_plan: None,
            abi_representation: mf.lambda_abi_representations.get(&lambda).cloned(),
            abi_logical_type: None,
            rep_arena: mf.rep_arena.clone(),
            global_plan: mf.global_plan.clone(),
            statics: Vec::new(),
            // Reads were recorded while lowering the whole pre-split fn, so the
            // parent's set covers every lambda region; carrying it is at worst
            // an over-invalidation within this fn's own hoists.
            comptime_reads: mf.comptime_reads.clone(),
            struct_registry_roots: mf.struct_registry_roots.clone(),
            insts: mf.insts.clone(),
            cells: mf.cells.clone(),
            blocks: mf.blocks.clone(),
            emit_ids: mf.emit_ids.clone(),
            inline_cleanup_load_cells: mf.inline_cleanup_load_cells.clone(),
            entry: tree.scopes[sid].entry,
            is_coroutine: false,
            resume_arg: None,
            coro_defers: Vec::new(),
            for_iter_headers: mf.for_iter_headers.clone(),
            bound_trees: mf.bound_trees.clone(),
            resolved_bound_trees: mf.resolved_bound_trees.clone(),
            // The split is a full arena clone sharing `mf`'s `Idx` space, so the
            // parent's keys stay valid and the entries for this lambda's own calls
            // come across with them. Dropping them stranded every specialized call
            // inside a lambda: the binding stayed on the parent (keyed to an inst
            // whose only reachable home is now here), which the parent could not
            // splice and the split body no longer knew about. Entries outside this
            // body's region address unreachable blocks and are ignored -- both the
            // inliner and the residual-binding check work on reachable blocks.
            spec_bindings: mf.spec_bindings.clone(),
            bound_next_split: mf.bound_next_split.clone(),
            nested_fns: Vec::new(),
            // Same arena clone, same `Idx` space: the parent's spans address the
            // instructions this body now owns.
            inst_spans: mf.inst_spans.clone(),
            inst_logical_types: mf.inst_logical_types.clone(),
            // Same arena clone, same `Idx` space -- and the same reason the
            // `spec_bindings` above come across: this lambda's own calls are
            // keyed to insts whose only reachable home is now this body, so
            // dropping them would strand their site identity here.
            call_sites: mf.call_sites.clone(),
            callee_instances: mf.callee_instances.clone(),
            call_abi_plans: mf.call_abi_plans.clone(),
            call_rep_plans: mf.call_rep_plans.clone(),
            flat_call_lanes: mf.flat_call_lanes.clone(),
            closure_env_plans: mf.closure_env_plans.clone(),
            fn_value_symbols: mf.fn_value_symbols.clone(),
            hoisted_fn_globals: mf.hoisted_fn_globals.clone(),
            flat_variant_source: None,
            // The owner table comes across with the entries that index it;
            // cloning one without the other would repoint every site.
            site_owners: mf.site_owners.clone(),
        });
    }
    // APPEND, don't overwrite: lowering already hoisted the enclosing fn's named
    // nested `fn`s into `mf.nested_fns`, and clobbering them would drop those
    // definitions from the chunk. Detached regions use the same sibling shape.
    mf.nested_fns.extend(nested);
}

/// Delay a local's box until its first store only when no access or capture can
/// observe the old entry box, and the allocation cannot repeat within the scope.
fn seeded_box_initializers(
    mf: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    boxed: &HashSet<Idx<Cell>>,
    lambda_sid: &HashMap<Idx<Inst>, ScopeId>,
) -> HashMap<Idx<Cell>, Idx<Inst>> {
    let locals: HashSet<_> = boxed
        .iter()
        .copied()
        .filter(|c| !tree.scopes[own.owner[c]].params.contains(c) && !own.loop_bindings.contains(c))
        .collect();
    if locals.is_empty() {
        return HashMap::default();
    }
    let mut accesses: HashMap<Idx<Cell>, Vec<(Idx<Block>, usize, Idx<Inst>)>> = HashMap::default();
    for (b, block) in mf.blocks.iter() {
        let Some(s) = tree.scope_of_block(b) else {
            continue;
        };
        for (pos, &id) in block.insts.iter().enumerate() {
            let mut record = |c| {
                if locals.contains(&c) && own.owner.get(&c) == Some(&s) {
                    accesses.entry(c).or_default().push((b, pos, id));
                }
            };
            mf.insts[id].cells_each(&mut record);
            if let Some(&child) = lambda_sid.get(&id) {
                for &c in &own.captures[child] {
                    record(c);
                }
            }
        }
    }
    let mut seeded = HashMap::default();
    let mut dominators = HashMap::default();
    let mut cyclic = HashMap::default();
    for (c, sites) in accesses {
        let owner = own.owner[&c];
        let scope = &tree.scopes[owner];
        for &(b, pos, id) in &sites {
            if !matches!(mf.insts[id], Inst::Store { cell, .. } if cell == c) {
                continue;
            }
            let dominates_all = sites.iter().all(|&(other, other_pos, _)| {
                if b == other {
                    pos <= other_pos
                } else {
                    let idom = dominators
                        .entry(owner)
                        .or_insert_with(|| skull_mir::cfg::dominators(mf, scope.entry));
                    skull_mir::cfg::dominates(idom, b, other)
                }
            });
            if !dominates_all {
                continue;
            }
            // Entry allocation already repeats on entry backedges. Moving into
            // any other cycle would split a previously shared cell per iteration.
            if b != scope.entry
                && *cyclic.entry(b).or_insert_with(|| {
                    let mut pending = Vec::new();
                    skull_mir::cfg::successors(&mf.blocks[b].term, |s| pending.push(s));
                    let mut seen = HashSet::default();
                    while let Some(next) = pending.pop() {
                        if next == b {
                            return true;
                        }
                        if seen.insert(next) {
                            skull_mir::cfg::successors(&mf.blocks[next].term, |s| pending.push(s));
                        }
                    }
                    false
                })
            {
                continue;
            }
            seeded.insert(c, id);
            break;
        }
    }
    seeded
}

fn frame_schema(frame: Option<&FrameInfo>) -> Option<FrameSchema> {
    frame.map(|frame| {
        let mut slots: Vec<u32> = frame.slots.values().copied().collect();
        slots.sort_unstable();
        FrameSchema { slots }
    })
}

fn refresh_owner_compatibility(
    mf: &mut MirFunc,
    frame: Option<&FrameInfo>,
    topology: LambdaTopologyFingerprint,
) {
    if mf.code_identity.is_none() {
        return;
    }
    mf.code_compatibility = Some(CodeCompatibility {
        call_abi: mf
            .code_abi_fingerprint
            .clone()
            .unwrap_or_else(|| "missing-code-abi".to_string()),
        capture: CaptureSchema::empty(),
        frame: frame_schema(frame),
        lambda_topology: topology,
    });
}

fn capture_schema(
    mf: &MirFunc,
    captures: &[Idx<Cell>],
    boxed: &HashSet<Idx<Cell>>,
    frame: Option<&FrameInfo>,
) -> CaptureSchema {
    let mut complete = true;
    let mut seen = HashSet::default();
    let fields = captures
        .iter()
        .map(|cell| {
            let identity = mf.cells[*cell].capture_identity.clone();
            if identity
                .as_ref()
                .is_none_or(|identity| !seen.insert(identity.clone()))
            {
                complete = false;
            }
            let mode = if frame.is_some_and(|f| f.s_cell == *cell || f.fields.contains(cell)) {
                CaptureMode::Frame
            } else if boxed.contains(cell) {
                CaptureMode::Shared
            } else {
                CaptureMode::ByValue
            };
            CaptureField {
                identity: identity.unwrap_or(CaptureIdentity::Generated {
                    role: Ustr::from("<unknown-capture>"),
                }),
                mode,
            }
        })
        .collect();
    CaptureSchema { fields, complete }
}

/// Materialize `e.c<S>` -- a load of the env record `env` then its `s`-cell field
/// -- the base a frame-cell access composes `.c<slot>` onto inside a hoisted body
/// (D3). Helper insts are appended to `new`; returns the `Field` inst.
fn env_frame_base(
    mf: &mut MirFunc,
    plan: &ClosureEnvPlan,
    s_cell: Idx<Cell>,
    new: &mut Vec<Idx<Inst>>,
) -> Idx<Inst> {
    let env = plan.env_cell().expect("frame capture has an env plan");
    let il = mf.alloc_inst(Inst::Load(env));
    new.push(il);
    let f = mf.alloc_inst(Inst::Field {
        base: il,
        field: field_of(plan, s_cell),
    });
    new.push(f);
    f
}

/// The inst yielding cell `cc`'s SLOT (helper used by the record-field lowering). (the box TABLE if boxed, else the plain
/// value) as seen from scope `s`, WITHOUT the `.v` projection -- used as an
/// env-record field value (a boxed capture threads the shared table; a by-value
/// capture snapshots the value). Helper insts are appended to `new`.
fn slot_in_scope(
    mf: &mut MirFunc,
    own: &Ownership,
    env_plans: &HashMap<ScopeId, ClosureEnvPlan>,
    s: ScopeId,
    cc: Idx<Cell>,
    new: &mut Vec<Idx<Inst>>,
) -> Idx<Inst> {
    // `s_cell` (the frame's synthetic state param, pushed into a lambda's caps by
    // the D2 collapse) is ROOT-owned but has NO owner in the reused transform tree
    // -- that tree is built pre-yield-rewrite, before `s_cell` is ever used. Treat
    // owner-None as ROOT: the only owner-None cell that reaches here is `s_cell`
    // (ghost cells never enter a capture set). So at ROOT it reads as the step's
    // param local (`Load`), and at a nested scope it threads via the env (whose
    // existence is guaranteed -- the nested scope captured the real frame cell
    // that collapsed to `s`, so it has an env).
    let owner = own.owner.get(&cc).copied().unwrap_or(crate::scope::ROOT);
    if owner == s {
        let l = mf.alloc_inst(Inst::Load(cc));
        new.push(l);
        l
    } else {
        // Transitive capture: threads through THIS scope's env.
        let env = env_plans[&s]
            .env_cell()
            .expect("transitive capture has an env plan");
        let il = mf.alloc_inst(Inst::Load(env));
        new.push(il);
        let f = mf.alloc_inst(Inst::Field {
            base: il,
            field: field_of(&env_plans[&s], cc),
        });
        new.push(f);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower;

    #[test]
    fn closure_conversion_exposes_one_owned_environment_plan() {
        let mut func = lower(
            "fn f() -> int { let captured = 3; let g = () => captured; return g() }",
            "f",
        );
        assert!(
            func.insts
                .iter()
                .any(|(_, inst)| matches!(inst, Inst::Lambda { .. })),
            "the closure fixture must have a nonzero lambda population"
        );
        convert_closures(&mut func, "f");
        assert_eq!(
            func.closure_env_plans.len(),
            1,
            "one lambda must have one env plan"
        );
        let plan = func
            .closure_env_plans
            .values()
            .next()
            .expect("closure env plan");
        assert!(
            !plan.captures().is_empty(),
            "the closure environment must carry a capture"
        );
        assert!(
            plan.env_cell().is_some(),
            "a capturing closure must own an env cell"
        );
    }

    #[test]
    fn escaping_lambdas_get_distinct_owner_site_identities_and_census() {
        let mut func = lower(
            "fn f() -> int {\n\
                 let captured = true\n\
                 let a = () => captured\n\
                 let b = () => captured\n\
                 let c = () => 7\n\
                 if a() { return 1 }\n\
                 if b() { return 2 }\n\
                 return c()\n\
             }",
            "f",
        );
        let owner = match func.code_identity.clone().expect("concrete owner") {
            skull_mir::code_identity::CodeIdentity::Named(instance) => instance,
            other => panic!("unexpected owner identity: {other:?}"),
        };
        convert_closures(&mut func, "f");
        let artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&func);
        let lambdas: Vec<_> = artifact
            .entries
            .keys()
            .filter_map(|identity| match identity {
                skull_mir::code_identity::CodeIdentity::Lambda { enclosing, site }
                    if *enclosing
                        == skull_mir::code_identity::CodeOwner::Instance(owner.clone()) =>
                {
                    Some(*site)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lambdas.len(),
            3,
            "all escaping lambda bodies need identities"
        );
        assert_eq!(
            lambdas
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "source sites, not ordinals, distinguish identical-looking lambdas"
        );
        assert_eq!(artifact.census.lambda_values, 3);
        assert_eq!(artifact.census.capturing_lambdas, 2);
        assert_eq!(artifact.census.capture_free_lambdas, 1);
        assert!(
            artifact.census.is_total(),
            "identity census: {:?}",
            artifact.census
        );
    }

    #[test]
    fn lambda_topology_rejects_insertion_but_body_edits_keep_identity() {
        use skull_mir::code_identity::{
            CaptureSchema, CodeArtifact, CodeCompatibility, CodeEntry, CodeIdentity, LambdaSiteId,
            LambdaTopologyFingerprint,
        };
        use skull_mir::instance::Instance;

        let owner = Instance::base(skull_mir::mir::FuncRef {
            module: bone_semantic::prelude::ModuleId::builtin(),
            func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from_u32(0)),
        });
        let site_a = LambdaSiteId {
            file: Ustr::from("f.rock"),
            offset: 10,
        };
        let site_b = LambdaSiteId {
            file: Ustr::from("f.rock"),
            offset: 20,
        };
        let topology = LambdaTopologyFingerprint::from_sites(&[(site_a, 0), (site_b, 0)]);
        let inserted = LambdaTopologyFingerprint::from_sites(&[
            (site_a, 0),
            (
                LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 15,
                },
                0,
            ),
            (site_b, 0),
        ]);
        let removed = LambdaTopologyFingerprint::from_sites(&[(site_b, 0)]);
        let compat = |topology| CodeCompatibility {
            call_abi: "params=[];result=none;lanes=0;scalar=false".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology: topology,
        };
        let id = CodeIdentity::Lambda {
            enclosing: skull_mir::code_identity::CodeOwner::Instance(owner.clone()),
            site: site_b,
        };
        let mut old = CodeArtifact::default();
        old.insert(CodeEntry {
            identity: id.clone(),
            compatibility: compat(topology),
            body_fingerprint: 1,
            body_name: Some(Ustr::from("old")),
            escaping: true,
            capture_free: true,
        });
        let mut body_edit = CodeArtifact::default();
        body_edit.insert(CodeEntry {
            identity: id.clone(),
            compatibility: compat(topology),
            body_fingerprint: 2,
            body_name: Some(Ustr::from("edited")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            old.compatible_overlay(&body_edit),
            "body-only edit must retain the slot"
        );
        let mut topology_edit = CodeArtifact::default();
        topology_edit.insert(CodeEntry {
            identity: id,
            compatibility: compat(inserted),
            body_fingerprint: 3,
            body_name: Some(Ustr::from("inserted")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            !old.compatible_overlay(&topology_edit),
            "insertion before a lambda must fail closed"
        );
        let mut removal_edit = CodeArtifact::default();
        removal_edit.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: skull_mir::code_identity::CodeOwner::Instance(owner.clone()),
                site: site_b,
            },
            compatibility: compat(removed),
            body_fingerprint: 4,
            body_name: Some(Ustr::from("removed")),
            escaping: true,
            capture_free: true,
        });
        assert!(
            !old.compatible_overlay(&removal_edit),
            "removing a lambda must fail closed"
        );

        let mut capture_edit = CodeArtifact::default();
        let mut incompatible_capture = CaptureSchema::empty();
        incompatible_capture.complete = false;
        capture_edit.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: skull_mir::code_identity::CodeOwner::Instance(owner),
                site: site_b,
            },
            compatibility: CodeCompatibility {
                capture: incompatible_capture,
                ..compat(topology)
            },
            body_fingerprint: 5,
            body_name: Some(Ustr::from("capture")),
            escaping: true,
            capture_free: false,
        });
        assert!(
            !old.compatible_overlay(&capture_edit),
            "incomplete capture schema must fail closed"
        );
    }

    #[test]
    fn owner_topology_ignores_non_lambda_entries_without_topology() {
        use skull_mir::code_identity::{
            CaptureSchema, CodeArtifact, CodeCompatibility, CodeEntry, CodeIdentity, CodeOwner,
            LambdaSiteId, LambdaTopologyFingerprint,
        };
        use skull_mir::instance::Instance;

        let owner = Instance::base(skull_mir::mir::FuncRef {
            module: bone_semantic::prelude::ModuleId::builtin(),
            func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from_u32(0)),
        });
        let owner = CodeOwner::Instance(owner);
        let topology = LambdaTopologyFingerprint::from_sites(&[(
            LambdaSiteId {
                file: Ustr::from("f.rock"),
                offset: 10,
            },
            0,
        )]);
        let compat = |lambda_topology| CodeCompatibility {
            call_abi: "unit".to_string(),
            capture: CaptureSchema::empty(),
            frame: None,
            lambda_topology,
        };
        let mut old = CodeArtifact::default();
        old.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 10,
                },
            },
            compatibility: compat(topology),
            body_fingerprint: 1,
            body_name: Some(Ustr::from("lambda_old")),
            escaping: true,
            capture_free: true,
        });
        old.insert(CodeEntry {
            identity: CodeIdentity::Nested {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 20,
                },
                name: Ustr::from("nested"),
            },
            compatibility: compat(LambdaTopologyFingerprint::default()),
            body_fingerprint: 2,
            body_name: Some(Ustr::from("nested_old")),
            escaping: true,
            capture_free: true,
        });
        let mut next = CodeArtifact::default();
        next.insert(CodeEntry {
            identity: CodeIdentity::Lambda {
                enclosing: owner.clone(),
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 10,
                },
            },
            compatibility: compat(topology),
            body_fingerprint: 3,
            body_name: Some(Ustr::from("lambda_new")),
            escaping: true,
            capture_free: true,
        });
        next.insert(CodeEntry {
            identity: CodeIdentity::Nested {
                enclosing: owner,
                site: LambdaSiteId {
                    file: Ustr::from("f.rock"),
                    offset: 20,
                },
                name: Ustr::from("nested"),
            },
            compatibility: compat(LambdaTopologyFingerprint::default()),
            body_fingerprint: 4,
            body_name: Some(Ustr::from("nested_new")),
            escaping: true,
            capture_free: true,
        });
        assert!(old.compatible_overlay(&next));
    }

    #[test]
    fn named_values_are_counted_but_direct_calls_do_not_acquire_slots() {
        let mut direct = lower(
            "fn target(x: int) -> int { return x }\n\
             fn f() -> int { return target(1) }",
            "f",
        );
        convert_closures(&mut direct, "f");
        let direct_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&direct);
        assert_eq!(direct_artifact.census.named_values, 0);
        assert_eq!(direct_artifact.census.direct_call_only, 1);

        let mut value = lower(
            "fn target(x: int) -> int { return x }\n\
             fn f() -> int { let callback = target; return callback(1) }",
            "f",
        );
        convert_closures(&mut value, "f");
        let value_artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&value);
        assert_eq!(value_artifact.census.named_values, 1);
        assert_eq!(
            value_artifact.census.unresolved_body, 1,
            "the per-body artifact must expose an unjoined target"
        );
        assert!(
            !value_artifact.census.is_total(),
            "a partial artifact must not report a total census"
        );
    }

    #[test]
    fn foreign_function_values_remain_explicit_boundaries() {
        let func = lower(
            "inline(lua) fn foreign(x: int) -> int = \"return x\"\n\
             fn f() -> int { let callback = foreign; return callback(1) }",
            "f",
        );
        assert!(func.code_value_identities.is_empty());
        assert_eq!(
            func.fn_value_symbols.len(),
            1,
            "the foreign value still has a boundary symbol"
        );
    }
}
