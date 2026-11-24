//! A re-entered declaration can leave earlier bindings alive in escaped closures.

use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::cfg::successors;
use skull_mir::mir::*;

use crate::scope::{Ownership, ScopeTree};

#[derive(Clone, Copy)]
pub(crate) enum RepeatedCapture {
    Snapshot,
    Shared(Idx<Inst>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Site {
    block: Idx<Block>,
    pos: usize,
}

impl Site {
    fn inst(self, mf: &MirFunc) -> Option<Idx<Inst>> {
        mf.blocks[self.block].insts.get(self.pos).copied()
    }

    fn after(self) -> Self {
        Self {
            pos: self.pos + 1,
            ..self
        }
    }
}

#[derive(Default)]
struct Uses {
    values: HashMap<Idx<Inst>, Vec<Site>>,
    loads: HashMap<Idx<Cell>, Vec<Site>>,
}

impl Uses {
    fn of(mf: &MirFunc, tree: &ScopeTree) -> Self {
        let mut uses = Self::default();
        for (block, body) in mf.blocks.iter() {
            if tree.scope_of_block(block).is_none() {
                continue;
            }
            for (pos, &id) in body.insts.iter().enumerate() {
                let site = Site { block, pos };
                if let Inst::Load(cell) = mf.insts[id] {
                    uses.loads.entry(cell).or_default().push(site);
                }
                mf.insts[id].uses_each(&mut |value| uses.record(mf, value, site));
            }
            let site = Site {
                block,
                pos: body.insts.len(),
            };
            body.term
                .uses_each(&mut |value| uses.record(mf, value, site));
        }
        uses
    }

    fn record(&mut self, mf: &MirFunc, value: Idx<Inst>, site: Site) {
        if matches!(mf.insts[value], Inst::Lambda { .. } | Inst::Load(_)) {
            self.values.entry(value).or_default().push(site);
        }
    }

    fn may_escape(
        &self,
        mf: &MirFunc,
        tree: &ScopeTree,
        own: &Ownership,
        creator: Site,
        lifetime: Site,
    ) -> bool {
        let scope = tree.scope_of_block(creator.block);
        let mut values = vec![(creator, creator.inst(mf).expect("lambda site"))];
        let mut seen = HashSet::default();
        while let Some((definition, value)) = values.pop() {
            if !seen.insert(value) {
                continue;
            }
            for &use_site in self.values.get(&value).into_iter().flatten() {
                if tree.scope_of_block(use_site.block) != scope {
                    return true;
                }
                let Some(user) = use_site.inst(mf) else {
                    return true; // return or yield
                };
                match &mf.insts[user] {
                    Inst::CallValue { callee, args }
                    | Inst::CallValueMulti { callee, args, .. }
                        if *callee == value && !args.contains(&value) => {}
                    Inst::CallValue { callee, args }
                        if args.as_slice() == [value]
                            && matches!(mf.insts[*callee], Inst::Global(n) if n.as_str() == DEFER_PUSH) =>
                    {
                        // The compiler pops the cleanup before leaving its lexical scope.
                    }
                    Inst::Store { cell, .. }
                        if mf.binding_initializers.contains(&user)
                            && own.owner(*cell) == scope
                            && own.single_write(*cell)
                            && !mf.cells[*cell].borrowed =>
                    {
                        values.extend(
                            self.loads
                                .get(cell)
                                .into_iter()
                                .flatten()
                                .map(|&load| (use_site, load.inst(mf).expect("load site"))),
                        );
                    }
                    _ => return true,
                }
                // Re-entering the declaration must not expose a call or alias
                // of the previous closure before its value is defined again.
                if tree.scope_of_block(lifetime.block) == scope
                    && reaches(mf, lifetime.after(), Some(definition), |site| {
                        site == use_site
                    })
                {
                    return true;
                }
            }
        }
        false
    }
}

pub(crate) fn repeated_captures(
    mf: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    detached_writes: Option<&HashSet<Idx<Cell>>>,
) -> HashMap<Idx<Cell>, RepeatedCapture> {
    let mut result = HashMap::default();
    if mf.binding_initializers.is_empty() || tree.scopes.len() == 1 {
        return result;
    }

    let mut captures: HashMap<Idx<Cell>, Vec<Site>> = HashMap::default();
    for (sid, scope) in tree.scopes.iter().enumerate() {
        let Some((lambda, block)) = scope.lambda.zip(scope.creation_block) else {
            continue;
        };
        let pos = mf.blocks[block]
            .insts
            .iter()
            .position(|&id| id == lambda)
            .expect("creation block contains its lambda");
        for &cell in &own.captures[sid] {
            if own.single_write(cell)
                && !(detached_writes.is_some() && own.owner(cell) == Some(crate::scope::ROOT))
            {
                continue; // already captured by value
            }
            captures.entry(cell).or_default().push(Site { block, pos });
        }
    }
    if captures.is_empty() {
        return result;
    }
    let mut cyclic = HashMap::default();
    let uses = std::cell::OnceCell::new();
    for (block, body) in mf.blocks.iter() {
        let Some(scope) = tree.scope_of_block(block) else {
            continue;
        };
        for (pos, &id) in body.insts.iter().enumerate() {
            let Inst::Store { cell, .. } = mf.insts[id] else {
                continue;
            };
            let Some(creators) = captures.get(&cell) else {
                continue;
            };
            if !mf.binding_initializers.contains(&id)
                || own.owner(cell) != Some(scope)
                || !*cyclic.entry(block).or_insert_with(|| is_cyclic(mf, block))
            {
                continue;
            }
            let init = Site { block, pos };
            let escapes = creators.iter().any(|&creator| {
                uses.get_or_init(|| Uses::of(mf, tree))
                    .may_escape(mf, tree, own, creator, init)
            });
            if !escapes {
                continue;
            }

            let mutated = own.written_outside(cell, scope)
                || detached_writes.is_some_and(|writes| writes.contains(&cell))
                || !own.single_write(cell)
                    && creators.iter().copied().any(|creator| {
                        tree.scope_of_block(creator.block) == Some(scope)
                            && reaches(mf, creator.after(), Some(init), |site| {
                                site.inst(mf).is_some_and(|id| {
                                    let mut writes = false;
                                    mf.insts[id].cell_writes_each(&mut |dst| writes |= dst == cell);
                                    writes
                                })
                            })
                    });
            let capture = if mutated {
                RepeatedCapture::Shared(id)
            } else {
                RepeatedCapture::Snapshot
            };
            result.insert(cell, capture);
        }
    }
    result
}

fn reaches(
    mf: &MirFunc,
    start: Site,
    stop: Option<Site>,
    mut target: impl FnMut(Site) -> bool,
) -> bool {
    let mut pending = vec![start];
    let mut seen = HashSet::default();
    while let Some(Site { block, pos }) = pending.pop() {
        let end = mf.blocks[block].insts.len();
        let mut cut_off = false;
        for pos in pos..=end {
            let site = Site { block, pos };
            if stop == Some(site) {
                cut_off = true;
                break;
            }
            if target(site) {
                return true;
            }
        }
        if !cut_off {
            successors(&mf.blocks[block].term, |block| {
                if seen.insert(block) {
                    pending.push(Site { block, pos: 0 });
                }
            });
        }
    }
    false
}

pub(crate) fn is_cyclic(mf: &MirFunc, block: Idx<Block>) -> bool {
    let mut pending = Vec::new();
    let mut seen = HashSet::default();
    successors(&mf.blocks[block].term, |next| pending.push(next));
    while let Some(next) = pending.pop() {
        if next == block {
            return true;
        }
        if seen.insert(next) {
            successors(&mf.blocks[next].term, |next| pending.push(next));
        }
    }
    false
}
