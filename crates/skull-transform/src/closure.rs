//! Closure conversion as a shared MIR->MIR pass
//! (`docs/closure-conversion-mir-plan.md` S1). Replaces the per-backend closure
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
//! `MakeEnv`/`Global`, never an `Inst::Lambda` -- except one nested in a co-fn
//! hook/defer body, a detached region this pass leaves for the inline arm
//! (`FrameCtx::hoist_lambdas == false`; see `convert_closures_framed`).

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::Idx;
use crate::scope::{non_dominating_by_value_captures, Ownership, ScopeId, ScopeTree};
use skull_mir::cfg::reachable_blocks_from;
use skull_mir::mir::*;
use ustr::Ustr;

fn raw<T>(i: Idx<T>) -> u32 {
    i.into_raw().into()
}

/// The env-record field name a captured cell threads under (`c<raw>`), shared by
/// the creation-site record and the hoisted body's env access.
fn field_of(c: Idx<Cell>) -> Ustr {
    Ustr::from(format!("c{}", raw(c)).as_str())
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
/// access inside a hoisted lambda body materializes `e.c<S>.c<slot>` (D3 of
/// `docs/coroutine-frame-lambda-plan.md`).
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
/// hoisted body materializes `e.c<S>.c<slot>` (D3). Detached hook/defer lambdas
/// (unreachable from the step entry) are left untouched -- their bodies still
/// emit through the inline arm (`FrameCtx::hoist_lambdas == false`), a sanctioned
/// exception pinned by `cofn_lambda_in_defer_body` (`docs/coroutine-frame-lambda-plan.md`).
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
    if !mf.insts.iter().any(|(_, i)| matches!(i, Inst::Lambda { .. })) {
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
    // The dominance rail wants a `FrameCtx` (it reads only `.fields`); build one
    // from the `FrameInfo` when converting a step.
    let fctx = frame.map(|f| skull_codegen_lua::FrameCtx {
        s_cell: f.s_cell,
        fields: f.fields.clone(),
        slots: f.slots.clone(),
        hoist_lambdas: true,
    });
    let mut boxed: HashSet<Idx<Cell>> = tree.boxed_cells(own);
    // Frame cells are table-mediated (via `s` or a by-value loop-binding snapshot
    // of a frame field), never boxed -- matches codegen's `b.retain(!frame)`.
    if let Some(f) = frame {
        boxed.retain(|c| !f.fields.contains(c));
    }
    boxed.extend(non_dominating_by_value_captures(mf, tree, own, &boxed, fctx.as_ref()));

    // lambda inst -> its scope.
    let mut lambda_sid: HashMap<Idx<Inst>, ScopeId> = HashMap::default();
    for (sid, s) in tree.scopes.iter().enumerate() {
        if let Some(lid) = s.lambda {
            lambda_sid.insert(lid, sid);
        }
    }
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

    // Per live lambda scope: hoisted name, sorted captures, an env param cell
    // (only when it captures anything -- a capture-free body takes no env).
    let mut hoisted_name: HashMap<ScopeId, Ustr> = HashMap::default();
    let mut env_cell: HashMap<ScopeId, Idx<Cell>> = HashMap::default();
    let mut captures_sorted: HashMap<ScopeId, Vec<Idx<Cell>>> = HashMap::default();
    // Post-partition capture set per scope, for O(1) membership in the block loop:
    // a member is captured DIRECTLY (env field `e.c<raw>`); a frame cell NOT in the
    // set (collapsed to `s`) reaches via `e.c<S>.c<slot>` instead.
    let mut caps_set: HashMap<ScopeId, HashSet<Idx<Cell>>> = HashMap::default();
    for &(k, sid) in &ordered {
        hoisted_name.insert(sid, Ustr::from(format!("{name_base}__lambda{k}").as_str()));
        let mut caps: Vec<Idx<Cell>> = own.captures[sid].iter().copied().collect();
        // Frame partition (D2 + D4(b), docs/coroutine-frame-lambda-plan.md), ported
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
            if via_s {
                caps.push(f.s_cell);
            }
        }
        caps.sort_by_key(|c| raw(*c));
        if !caps.is_empty() {
            env_cell.insert(sid, mf.new_cell(Some(Ustr::from("__rock_env"))));
        }
        caps_set.insert(sid, caps.iter().copied().collect());
        captures_sorted.insert(sid, caps);
    }

    // --- Rewrite every scoped block: captured/boxed cell accesses become env /
    // `.v` projections, and each `Inst::Lambda` creation site becomes its record
    // (or a bare global). Helper insts are spliced immediately before the inst
    // that reads them; the original id is REPURPOSED as the final projection so
    // every existing reference to it resolves to the projected value. ---
    let block_ids: Vec<Idx<Block>> = mf.blocks.iter().map(|(b, _)| b).collect();
    for b in block_ids {
        let Some(s) = tree.scope_of_block(b) else { continue }; // dead-lambda block
        let orig = mf.blocks[b].insts.clone();
        let mut new: Vec<Idx<Inst>> = Vec::with_capacity(orig.len());
        for id in orig {
            match mf.insts[id].clone() {
                Inst::Load(c) => {
                    let captured = caps_set.get(&s).is_some_and(|cs| cs.contains(&c));
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
                        let sbase = env_frame_base(mf, env_cell[&s], f.s_cell, &mut new);
                        mf.insts[id] = Inst::Field { base: sbase, field: frame_field(f.slots[&c]) };
                    } else if captured {
                        // A boxed cell reads `.v` on its box table; a by-value
                        // capture is the env field directly.
                        let il = mf.insts.alloc(Inst::Load(env_cell[&s]));
                        new.push(il);
                        if boxed_c {
                            let f = mf.insts.alloc(Inst::Field { base: il, field: field_of(c) });
                            new.push(f);
                            mf.insts[id] = Inst::Field { base: f, field: Ustr::from("v") };
                        } else {
                            mf.insts[id] = Inst::Field { base: il, field: field_of(c) };
                        }
                    } else {
                        // Local boxed: `.v` on a load of the box table.
                        let l = mf.insts.alloc(Inst::Load(c));
                        new.push(l);
                        mf.insts[id] = Inst::Field { base: l, field: Ustr::from("v") };
                    }
                    new.push(id);
                }
                Inst::Store { cell: c, value } => {
                    let boxed_c = boxed.contains(&c);
                    let captured = caps_set.get(&s).is_some_and(|cs| cs.contains(&c));
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
                        let sbase = env_frame_base(mf, env_cell[&s], f.s_cell, &mut new);
                        (sbase, frame_field(f.slots[&c]))
                    } else if captured {
                        let il = mf.insts.alloc(Inst::Load(env_cell[&s]));
                        new.push(il);
                        let f = mf.insts.alloc(Inst::Field { base: il, field: field_of(c) });
                        new.push(f);
                        (f, Ustr::from("v"))
                    } else {
                        let l = mf.insts.alloc(Inst::Load(c));
                        new.push(l);
                        (l, Ustr::from("v"))
                    };
                    mf.insts[id] = Inst::StoreField { base, field, value };
                    new.push(id);
                }
                Inst::Lambda { .. } => {
                    let child = lambda_sid[&id];
                    let caps = &captures_sorted[&child];
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
                        let slot = slot_in_scope(mf, own, &env_cell, s, cc, &mut new);
                        fields.push((field_of(cc), slot));
                    }
                    mf.insts[id] = Inst::MakeEnv { fn_ref: Some(hoisted_name[&child]), fields };
                    new.push(id);
                }
                _ => new.push(id),
            }
        }
        mf.blocks[b].insts = new;
    }

    // --- Materialize each boxed cell as a box table created once at its owner
    // scope's entry: `{}` for a plain local, `{ v = <param> }` for a boxed param
    // (the box seeds from the incoming arg before it is overwritten). ---
    let mut entry_prepend: HashMap<Idx<Block>, Vec<Idx<Inst>>> = HashMap::default();
    // Sorted so the created boxes / declarations are traversal-independent.
    let mut boxed_sorted: Vec<Idx<Cell>> = boxed.iter().copied().collect();
    boxed_sorted.sort_by_key(|c| raw(*c));
    for c in boxed_sorted {
        let owner = own.owner[&c];
        let entry = tree.scopes[owner].entry;
        let is_param = tree.scopes[owner].params.contains(&c);
        let dest = entry_prepend.entry(entry).or_default();
        if is_param {
            let seed = mf.insts.alloc(Inst::Load(c));
            let boxv = mf.insts.alloc(Inst::MakeEnv { fn_ref: None, fields: vec![(Ustr::from("v"), seed)] });
            let st = mf.insts.alloc(Inst::Store { cell: c, value: boxv });
            dest.extend([seed, boxv, st]);
        } else {
            let boxv = mf.insts.alloc(Inst::MakeEnv { fn_ref: None, fields: Vec::new() });
            let st = mf.insts.alloc(Inst::Store { cell: c, value: boxv });
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
    // Under a frame the DETACHED hook/defer blocks (unreachable from the step
    // entry) still carry `Inst::Lambda`s that their aux emission renders inline
    // (`FrameCtx::hoist_lambdas == false`), so they must survive -- neutralizing
    // them would break `cofn_lambda_in_defer_body` (docs/coroutine-frame-lambda-plan.md
    // D5). Only the frame-less path (ordinary/JS) nils dead residuals.
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
        let mut params: Vec<Idx<Cell>> = Vec::new();
        if let Some(&e) = env_cell.get(&sid) {
            params.push(e);
        }
        params.extend(tree.scopes[sid].params.iter().copied());
        nested.push(MirFunc {
            name: hoisted_name[&sid],
            params,
            statics: Vec::new(),
            insts: mf.insts.clone(),
            cells: mf.cells.clone(),
            blocks: mf.blocks.clone(),
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
            // Same arena clone, same `Idx` space -- and the same reason the
            // `spec_bindings` above come across: this lambda's own calls are
            // keyed to insts whose only reachable home is now this body, so
            // dropping them would strand their site identity here.
            call_sites: mf.call_sites.clone(),
            callee_instances: mf.callee_instances.clone(),
            call_abi_plans: mf.call_abi_plans.clone(),
            fn_value_symbols: mf.fn_value_symbols.clone(),
            hoisted_fn_globals: mf.hoisted_fn_globals.clone(),
            // The owner table comes across with the entries that index it;
            // cloning one without the other would repoint every site.
            site_owners: mf.site_owners.clone(),
        });
    }
    // APPEND, don't overwrite: lowering already hoisted the enclosing fn's named
    // nested `fn`s into `mf.nested_fns`, and clobbering them would drop those
    // definitions from the chunk (a real bug for the Lua path, where a fn can hold
    // both a named nested `fn` and a lambda). The named siblings keep their own
    // `Inst::Lambda`s, which the emitter converts on their own emission -- the pass
    // only splits THIS fn's direct lambdas.
    mf.nested_fns.extend(nested);
}

/// Materialize `e.c<S>` -- a load of the env record `env` then its `s`-cell field
/// -- the base a frame-cell access composes `.c<slot>` onto inside a hoisted body
/// (D3). Helper insts are appended to `new`; returns the `Field` inst.
fn env_frame_base(mf: &mut MirFunc, env: Idx<Cell>, s_cell: Idx<Cell>, new: &mut Vec<Idx<Inst>>) -> Idx<Inst> {
    let il = mf.insts.alloc(Inst::Load(env));
    new.push(il);
    let f = mf.insts.alloc(Inst::Field { base: il, field: field_of(s_cell) });
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
    env_cell: &HashMap<ScopeId, Idx<Cell>>,
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
        let l = mf.insts.alloc(Inst::Load(cc));
        new.push(l);
        l
    } else {
        // Transitive capture: threads through THIS scope's env.
        let il = mf.insts.alloc(Inst::Load(env_cell[&s]));
        new.push(il);
        let f = mf.insts.alloc(Inst::Field { base: il, field: field_of(cc) });
        new.push(f);
        f
    }
}
