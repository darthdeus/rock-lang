//! Closure conversion as a shared MIR->MIR pass
//! (`docs/closure-conversion-mir-plan.md` S1). Replaces the per-backend closure
//! machinery (codegen-lua's `HoistPlan`/`EnvCtx`; the JS T5/T7/T8c native-arrow
//! path) with ONE transform: every `Inst::Lambda` becomes either a bare
//! [`Inst::Global`] (capture-free) or an env-record [`Inst::MakeEnv`] (capturing),
//! and its sub-CFG is split out to a sibling [`MirFunc`] in `nested_fns`.
//!
//! The capture SEMANTICS (by-value vs box, per-iteration loop bindings, the
//! dominance rail) are NOT re-decided here: this pass REUSES the exact
//! classification the Lua codegen path already ships
//! (`skull_codegen_lua::scope` + `non_dominating_by_value_captures`), so there is
//! one mechanism and no divergence. What is new is that the analysis result is
//! MATERIALIZED as MIR (`MakeEnv` records, `Field`/`StoreField` env access, box
//! tables) instead of being consumed at render time.
//!
//! Wired into the JS pipeline only this slice (`docs/...` S1); the Lua path still
//! converts at codegen until the S2 cutover, so `MakeEnv` never reaches Lua
//! emission (asserted there).

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::Idx;
use skull_codegen_lua::scope::{Ownership, ScopeId, ScopeTree};
use skull_codegen_lua::{non_dominating_by_value_captures, reachable_blocks_from};
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

/// Convert every closure in `mf` to a defunctionalized record + a hoisted sibling
/// fn. `name_base` is the enclosing fn's UNIQUE emitted name (the JS name on this
/// slice; the mangled Lua name at S2): hoisted bodies are `<name_base>__lambda<k>`
/// (`k` = the `Inst::Lambda`'s inst-arena order). Post-pass no `Inst::Lambda`
/// survives (asserted); the enclosing region keeps its blocks (lambda regions go
/// unreachable in it) and gains `nested_fns`, one per live lambda.
///
/// DEVIATION from the plan's `convert_closures(mf)` signature: the unique name is
/// passed IN (the pass is MirFunc-local and has no name-mangling context of its
/// own). The caller supplies a collision-free base -- on JS that is
/// `callee_js_name`, itself overload-resolved -- so `<base>__lambda<k>` is unique
/// by construction.
pub fn convert_closures(mf: &mut MirFunc, name_base: &str) {
    if !mf.insts.iter().any(|(_, i)| matches!(i, Inst::Lambda { .. })) {
        return;
    }

    // --- Analysis: REUSE the Lua path's classification verbatim (frame = None:
    // JS has no coroutines). ---
    let tree = ScopeTree::build(mf);
    let own = tree.analyze(mf);
    let mut boxed: HashSet<Idx<Cell>> = tree.boxed_cells(&own);
    boxed.extend(non_dominating_by_value_captures(mf, &tree, &own, &boxed, None));

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
    for &(k, sid) in &ordered {
        hoisted_name.insert(sid, Ustr::from(format!("{name_base}__lambda{k}").as_str()));
        let mut caps: Vec<Idx<Cell>> = own.captures[sid].iter().copied().collect();
        caps.sort_by_key(|c| raw(*c));
        if !caps.is_empty() {
            env_cell.insert(sid, mf.new_cell(Some(Ustr::from("__rock_env"))));
        }
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
                    let owner = own.owner.get(&c).copied();
                    let captured = owner.is_some() && owner != Some(s);
                    let boxed_c = boxed.contains(&c);
                    if !captured && !boxed_c {
                        new.push(id); // local by-value: read the cell as-is
                        continue;
                    }
                    // Splice the helper insts (env load, field projection) then
                    // REPURPOSE `id` as the final read so every reference to it
                    // resolves to the projected value. A boxed cell reads `.v` on
                    // its box table; a by-value capture is the env field directly.
                    if captured {
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
                    if !boxed.contains(&c) {
                        new.push(id); // local plain store
                        continue;
                    }
                    let owner = own.owner.get(&c).copied();
                    let captured = owner.is_some() && owner != Some(s);
                    let base = if captured {
                        let il = mf.insts.alloc(Inst::Load(env_cell[&s]));
                        new.push(il);
                        let f = mf.insts.alloc(Inst::Field { base: il, field: field_of(c) });
                        new.push(f);
                        f
                    } else {
                        let l = mf.insts.alloc(Inst::Load(c));
                        new.push(l);
                        l
                    };
                    mf.insts[id] = Inst::StoreField { base, field: Ustr::from("v"), value };
                    new.push(id);
                }
                Inst::Lambda { .. } => {
                    let child = lambda_sid[&id];
                    let caps = &captures_sorted[&child];
                    if caps.is_empty() {
                        mf.insts[id] = Inst::Global(hoisted_name[&child]);
                        new.push(id);
                        continue;
                    }
                    // Record field values: each capture's SLOT (box table / value)
                    // read FROM this scope -- a transitive capture threads through
                    // this scope's own env.
                    let mut fields: Vec<(Ustr, Idx<Inst>)> = Vec::with_capacity(caps.len());
                    for &cc in caps {
                        let slot = slot_in_scope(mf, &own, &env_cell, s, cc, &mut new);
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
    for (_, inst) in mf.insts.iter_mut() {
        if matches!(inst, Inst::Lambda { .. }) {
            *inst = Inst::Const(Lit::Nil);
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
            bound_trees: Default::default(),
            spec_bindings: Default::default(),
            nested_fns: Vec::new(),
        });
    }
    mf.nested_fns = nested;
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
    if own.owner.get(&cc).copied() == Some(s) {
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
