use skull_parser::{Ast, FunctionBody, ItemKind, MacroRegistry, collect_registry, cook_one};

use bone_semantic::{
    hir::{Function, Module, TypeCtor, TypeExpr},
    prelude::*,
};

use crate::span_shift;

/// Unique identifier of a Body across all modules
type BodyId = (ModuleId, BodyOwner);

/// Represents a kind of update necessary after change detection
///
/// - NoChange: nothing really changed (e.g. watcher detected a file save without any changes)
/// - BodyOnly: body of some items changed, but their interface remains, meaning we can do a local
///   recompile of the bodies only
/// - Interface: a "public API" of an item changed, all of its users/callers need to be invalidated
///   and recompiled as well
/// - FullUser: a full recompile from scratch
///
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UpdateTier {
    NoChange,
    BodyOnly,
    Interface,
    FullUser,
}

/// Result of a session update. Stores the type of update, and all the bodies
/// that were reinferred.
#[derive(Clone, Debug)]
pub struct UpdateResult {
    pub tier: UpdateTier,
    pub reinferred: Vec<BodyId>,
    pub relowered: Vec<BodyId>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub ws: Workspace,
    pub warm: Vec<Ast>,
    pub warm_mids: FxHashSet<ModuleId>,
    pub paths: Vec<String>,
    pub files: FxHashMap<String, UserFile>,
    /// Reverse dep index: name -> modules whose bodies reference it. Used by
    /// `rebuild_interface` to find which modules to re-infer on an interface edit.
    pub dependents: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Forward dep index: module -> the names its bodies reference. The inverse
    /// of `dependents`, kept so a body edit can update just the edited module's
    /// entry in O(its deps) instead of rescanning the whole workspace.
    pub module_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    /// Reverse index for INTERFACE-position type references: type-name -> modules
    /// whose signatures / globals / fields name it. The interface twin of
    /// `dependents` (which tracks body references). An interface change re-LOWERS
    /// these dependents (their signatures must re-resolve the name); a body change
    /// only re-infers `dependents`. Built from each module's lowered interface
    /// arena in `rebuild_dependents`.
    pub dependents_iface: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    pub registry: MacroRegistry,
}

#[derive(Clone, Debug)]
pub struct UserFile {
    pub text_hash: u64,
    pub item_prints: Vec<ItemPrint>,
    /// The file's current source text -- the single source of truth for this
    /// file's parse. A body edit byte-diffs old vs new to localize the change to
    /// one item (`try_incremental_body_edit`); the slow/full paths re-parse from
    /// it on demand. The AST is NOT persisted: the per-body HIR (`Module.bodies`)
    /// is the incremental store, so a stored AST would be a redundant second copy
    /// that drifts. `docs/skull-incremental-parse-plan.md`.
    pub text: String,
    /// The macros this file defines (Functions with `is_macro`), captured at
    /// parse time. Kept so an interface edit can ask "did a macro change?"
    /// (`touches_macro`) without a retained AST -- and so consumers (ctags) can
    /// read macro signatures, which are dropped from the cooked HIR by expansion.
    /// Each def's `span` is shifted on the fast body-edit path, so its line stays
    /// accurate without a reparse.
    pub macros: Vec<MacroDef>,
    /// Recovered syntax errors from this file's last parse. The parse is total
    /// (best-effort tree + recovery errors), so these are kept here and surfaced
    /// via `diagnostics()` -- a broken file still type-checks its valid parts AND
    /// shows the parse error as a diagnostic (the compile path rejects on them).
    pub syntax_errors: Vec<CompilerError>,
}

/// One parameter of a macro definition, rendered at parse time (the AST is
/// transient, so the type is stored as surface text rather than an arena index).
#[derive(Clone, Debug)]
pub struct MacroParam {
    pub name: Ustr,
    pub is_variadic: bool,
    pub ty: String,
}

/// A macro definition (`macro foo(...) { ... }`) recovered at parse time before
/// expansion drops it from the cooked module. Holds what navigation/ctags need:
/// name, definition name-span, parameter signature, and return type.
#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: Ustr,
    /// The macro NAME's span -- shifted incrementally on fast body edits.
    pub span: Span,
    pub params: Vec<MacroParam>,
    pub ret: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ItemKindTag {
    Fn,
    Struct,
    Enum,
    Newtype,
    Global,
    Decorator,
    Other,
}

#[derive(Clone, Debug)]
pub struct ItemPrint {
    pub kind: ItemKindTag,
    pub name: Ustr,
    pub iface_hash: u64,
    pub body_hash: Option<u64>,
    offset: usize,
    /// Full item span (covers leading annotations) -- the incremental body-edit
    /// fast path slices `text[item_span]` to re-parse just this item, and reads
    /// its line/col as the `parse_one_item` base. Maintained across fast edits
    /// (the changed item replaced; following items shifted by the byte delta).
    item_span: Span,
    /// Byte range of the function's body BLOCK (Rock-bodied fns only) -- an edit
    /// strictly inside it is a pure body edit. `None` for non-fn / lua / extern /
    /// macro items, which the fast path never takes.
    body_range: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub enum FileDiff {
    LayoutOnly,
    BodiesOnly(Vec<usize>),
    Interface,
}

pub fn item_prints(a: &Ast, text: &str) -> Vec<ItemPrint> {
    use skull_parser::ItemKind;
    use std::hash::{Hash, Hasher};

    let mut prints = Vec::new();

    for item in a.items.iter() {
        let (start, stop) = item.span.offset_range;
        debug_assert!(stop <= text.len(), "item span out of bounds");

        let mut h = rustc_hash::FxHasher::default();

        for ann in item.annotations.iter() {
            let (a, b) = ann.span.offset_range;
            text[a..b].hash(&mut h);
        }

        let (kind, name, body_hash, body_range) = match &item.kind {
            ItemKind::Function(f) => {
                let mut body_range = None;
                let body_hash = if let FunctionBody::Rock(b) = f.body && !f.is_macro {
                    let (bs, be) = a.b(&b).span.offset_range;

                    debug_assert!(start <= bs && be <= stop, "block escapes item span");

                    text[start..bs].hash(&mut h);
                    text[be..stop].hash(&mut h);

                    body_range = Some((bs, be));
                    Some(text_hash(&text[bs..be]))
                } else {
                    // lua/extern/macro, whole body is interface
                    text[start..stop].hash(&mut h);
                    None
                };

                (ItemKindTag::Fn, f.name.name, body_hash, body_range)
            }

            ItemKind::Struct(s) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Struct, s.name.name, None, None)
            }

            ItemKind::Enum(e) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Enum, e.name.name, None, None)
            }

            ItemKind::Newtype(n) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Newtype, n.name.name, None, None)
            }

            ItemKind::Decorator(d) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Newtype, d.name.name, None, None)
            }

            ItemKind::Global(g) => {
                // A `const`'s init VALUE is its body; the `const` keyword, name, and
                // type annotation are interface. So a const value-only edit (same
                // type) is a BodyOnly edit -> it hot-reloads through the cheap
                // overlay. A type or keyword change moves the prefix text into the
                // iface_hash -> Interface (full rebuild).
                //
                // A mutable `global` keeps WHOLE-item interface: the hot-reload guard
                // preserves its live value across the re-dofile, so a value edit has
                // no live effect and the overlay deliberately never re-emits it.
                // Whole-item iface keeps such an edit on the full-rebuild path.
                if g.is_const && let Some(value) = g.value {
                    let (vs, ve) = a.e(&value).span.offset_range;
                    debug_assert!(start <= vs && ve <= stop, "const init escapes item span");
                    text[start..vs].hash(&mut h);
                    text[ve..stop].hash(&mut h);
                    (ItemKindTag::Other, g.name.name, Some(text_hash(&text[vs..ve])), Some((vs, ve)))
                } else {
                    text[start..stop].hash(&mut h);
                    (ItemKindTag::Other, g.name.name, None, None)
                }
            }

            ItemKind::CompilerDirective(_) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Other, Ustr::from(""), None, None)
            }

            ItemKind::MacroInvocation { name, .. } => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Other, name.name, None, None)
            }

            ItemKind::Comment(_) => continue,
        };

        prints.push(ItemPrint {
            kind,
            name,
            iface_hash: h.finish(),
            body_hash,
            offset: start,
            item_span: item.span,
            body_range,
        });
    }

    prints
}

pub fn classify(old: &[ItemPrint], new: &[ItemPrint]) -> FileDiff {
    if old.len() != new.len() {
        return FileDiff::Interface;
    }

    let mut changed = Vec::new();

    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        if o.kind != n.kind || o.name != n.name || o.iface_hash != n.iface_hash {
            return FileDiff::Interface;
        }

        if o.body_hash != n.body_hash {
            changed.push(i);
        }
    }

    if changed.is_empty() {
        FileDiff::LayoutOnly
    } else {
        FileDiff::BodiesOnly(changed)
    }
}

pub fn name_shapes(prints: &[ItemPrint]) -> FxHashMap<Ustr, u64> {
    let mut by_name: FxHashMap<Ustr, Vec<u64>> = FxHashMap::default();

    for p in prints {
        if p.name.is_empty() {
            continue;
        }

        by_name.entry(p.name).or_default().push(p.iface_hash);
    }

    by_name
        .into_iter()
        .map(|(n, mut hs)| {
            hs.sort();
            (n, hash_u64s(&hs))
        })
        .collect()
}

pub fn changed_names(old: &FxHashMap<Ustr, u64>, new: &FxHashMap<Ustr, u64>) -> FxHashSet<Ustr> {
    old.keys()
        .chain(new.keys())
        .filter(|n| old.get(*n) != new.get(*n))
        .copied()
        .collect()
}

impl Session {
    pub fn new(warm: Vec<Ast>, files: Vec<(String, String)>) -> Result<Self, CompilerError> {
        let mut ws = Workspace::default();

        let warm_mids: FxHashSet<ModuleId> = warm
            .iter()
            .map(|m| ws.intern_module(m.file))
            .collect();

        let mut session = Session {
            ws,
            warm,
            warm_mids,
            paths: Vec::new(),
            files: FxHashMap::default(),
            dependents: Default::default(),
            module_dep_names: Default::default(),
            dependents_iface: Default::default(),
            registry: Default::default(),
        };

        // Parse each file once for its prints/macros AND keep the module to feed
        // lowering directly -- avoids a second parse via `combined_unexpanded`,
        // which re-parses (the AST is transient and not persisted).
        let mut user_modules: Vec<Ast> = Vec::new();
        for (path, text) in files {
            if session.files.contains_key(&path) {
                comp_error!(
                    Span::unknown(),
                    "bone session: duplicate file `{}` in initial set",
                    path
                );
            }

            let module = skull_parser::parse_module(&text, ustr(path.as_str()))?;

            let mid = session.ws.intern_module(module.file);
            if session.warm_mids.contains(&mid) {
                // A stdlib file present in the initial set IS the warm module --
                // skip the user copy (adding it would self-collide on every
                // stdlib type); the warm build already covers it.
                continue;
            }

            let prints = item_prints(&module, &text);
            let macros = macro_defs(&module);

            session.files.insert(
                path.clone(),
                UserFile {
                    text_hash: text_hash(&text),
                    item_prints: prints,
                    text,
                    macros,
                    syntax_errors: module.recovery_errors.clone(),
                },
            );

            session.paths.push(path);
            user_modules.push(module);
        }

        let mut expanded = session.warm.clone();
        expanded.extend(user_modules);
        session.registry = collect_registry(&expanded);
        expand_and_desugar(&mut session.ws, &mut expanded)?;

        lower_and_infer(&mut session.ws, &expanded)?;
        session.rebuild_dependents();

        Ok(session)
    }

    /// Re-parse every user file from its stored text and prepend the (immutable,
    /// pre-parsed) warm modules -- the un-expanded module set the full-rebuild
    /// path (`rebuild_users`) feeds to `expand_and_desugar`. The AST is transient
    /// (not persisted), so this re-parses; it runs only on the O(workspace)
    /// fallback, where a parse per file is in line with the from-scratch cost.
    pub fn combined_unexpanded(&self) -> Result<Vec<Ast>, CompilerError> {
        let mut combined = self.warm.clone();

        for path in &self.paths {
            combined.push(skull_parser::parse_module(&self.files[path].text, ustr(path))?);
        }

        Ok(combined)
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    /// Run `f` against the workspace with `path`'s text temporarily replaced by
    /// `variant`, then restore the original state. The speculative-analysis
    /// primitive for IDE paths (completion munges): the variant goes through the
    /// normal update tiers, so a body-local munge costs one body re-inference --
    /// not the from-scratch project pass `combined_unexpanded` + `semantic_pass`
    /// would be.
    ///
    /// An UNTRACKED `path` (a file this session refused because its live text
    /// never lowered clean, or one it has never seen) is speculatively tracked
    /// for the duration of `f` and removed again -- the session ends as
    /// untracked as it started.
    ///
    /// Not panic-safe: a panicking `f` leaves the variant applied (the IDE
    /// queries this serves don't panic). A failed variant update is restored
    /// before the error propagates; a failed RESTORE propagates itself -- the
    /// original text round-tripped before, so that failure is a real bug and
    /// must not be swallowed.
    pub fn with_file_variant<R>(
        &mut self,
        path: &str,
        variant: &str,
        f: impl FnOnce(&Workspace) -> R,
    ) -> Result<R, CompilerError> {
        let Some(original) = self.files.get(path).map(|uf| uf.text.clone()) else {
            self.update_file(path, variant)?;
            let out = f(&self.ws);
            self.remove_file(path)?;
            return Ok(out);
        };
        if let Err(e) = self.update_file(path, variant) {
            self.update_file(path, &original)?;
            return Err(e);
        }
        let out = f(&self.ws);
        self.update_file(path, &original)?;
        Ok(out)
    }

    pub fn update_file(&mut self, path: &str, text: &str) -> Result<UpdateResult, CompilerError> {
        // Reset the re-inference counter so callers can read EXACTLY how many
        // bodies this update re-inferred (check_module_incremental bumps it).
        self.ws
            .reinferred_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let hash = text_hash(text);

        if let Some(existing) = self.files.get(path) {
            if existing.text_hash == hash {
                return Ok(UpdateResult {
                    tier: UpdateTier::NoChange,
                    reinferred: Vec::new(),
                    relowered: Vec::new(),
                });
            }
        }

        // Fast path: a single edit localized to one Rock-bodied function's
        // interior re-parses ONLY that item (no whole-file reparse/clone). Bails
        // (Ok(None)) to the full parse below for anything non-localized; the
        // re-parsed item's interface hash is re-checked, so a mis-localized
        // interface edit also falls through.
        if let Some(result) = self.try_incremental_body_edit(path, text, hash)? {
            return Ok(result);
        }

        let module = skull_parser::parse_module(text, ustr(path))?;
        let syntax_errors = module.recovery_errors.clone();
        let mid = self.ws.intern_module(module.file);
        if self.warm_mids.contains(&mid) {
            // Opening a warm (stdlib) file: it IS the warm module, not a second
            // copy. No-op rather than adding a duplicate that would self-collide
            // on every stdlib type. Queries on this path resolve to the warm
            // module; its real diagnostics (if any) come from the warm build.
            return Ok(UpdateResult {
                tier: UpdateTier::NoChange,
                reinferred: Vec::new(),
                relowered: Vec::new(),
            });
        }

        let new_prints = item_prints(&module, text);
        let new_macros = macro_defs(&module);

        let diff = (self.files.contains_key(path) && !module.items.is_empty())
            .then(|| classify(&self.files[path].item_prints, &new_prints));

        let interfaces_changed = match &diff {
            Some(FileDiff::Interface) => {
                let changed = changed_names(
                    &name_shapes(&self.files[path].item_prints),
                    &name_shapes(&new_prints),
                );

                // A macro change rewrites its users' EXPANSION, so they must be
                // re-lowered (re-expanded), not merely re-inferred - which the
                // narrow rebuild_interface path cannot do. When a macro F
                // defines (old or new) is among the changed names, drop to the
                // full rebuild, which re-expands every user. `None` here routes
                // to the `_` (rebuild_users) arm below.
                if touches_macro(&self.files[path].macros, &new_macros, &changed) {
                    None
                } else {
                    Some(changed)
                }
            }
            _ => None,
        };

        let previous = self.files.insert(
            path.to_string(),
            UserFile {
                text_hash: hash,
                item_prints: new_prints,
                text: text.to_string(),
                macros: new_macros,
                syntax_errors,
            },
        );

        if previous.is_none() {
            self.paths.push(path.to_string());
        }

        let restore = move |s: &mut Self| match previous {
            Some(prev) => {
                s.files.insert(path.to_string(), prev);
            }

            None => {
                s.files.remove(path);
                s.paths.retain(|p| p != path);
            }
        };

        // The freshly parsed `module` is threaded into the chosen rebuild path
        // rather than stored: the AST is transient, so the rebuilder cooks+lowers
        // this owned copy directly (no clone-from-storage, no persisted AST).
        match (diff, interfaces_changed) {
            (Some(FileDiff::BodiesOnly(changed)), _) => {
                if changed.len() == 1 {
                    let idx = changed[0];

                    self.rebuild_or_revert("update", path, move |s| s.rebuild_body_incremental(path, module, idx), restore)
                } else {
                    self.rebuild_or_revert("update", path, move |s| s.rebuild_module(path, module, &changed), restore)
                }
            }

            (Some(FileDiff::LayoutOnly), _) => {
                self.rebuild_or_revert("update", path, move |s| s.rebuild_module(path, module, &[]), restore)
            }

            (Some(FileDiff::Interface), Some(changed)) => self.rebuild_or_revert(
                "update",
                path,
                move |s| s.rebuild_interface(module, &changed),
                restore,
            ),

            _ => self.rebuild_or_revert("update", path, |s| s.rebuild_users(), restore),
        }
    }

    fn rebuild_interface(
        &mut self,
        module: Ast,
        changed: &FxHashSet<Ustr>,
    ) -> Result<UpdateResult, CompilerError> {
        let file = module.file;
        let mid = self.ws.module_ids[&file];
        // `BONE_CASCADE_STATS=1` prints a per-stage breakdown of the interface
        // cascade (closure / re-parse / lower / re-infer / dep-index), the lens
        // for where Interface-tier latency goes. Mirrors `BONE_REINFER_STATS`.
        let stats = std::env::var("BONE_CASCADE_STATS").is_ok();
        let t_closure = Instant::now();

        // Type names the edited module defines AFTER this edit (from the fresh
        // AST). Diffed against its pre-edit types below to find removals/renames.
        let new_types: FxHashSet<Ustr> = module
            .items
            .iter()
            .filter_map(|it| match &it.kind {
                ItemKind::Struct(s) => Some(s.name.name),
                ItemKind::Enum(e) => Some(e.name.name),
                ItemKind::Newtype(n) => Some(n.name.name),
                _ => None,
            })
            .collect();

        // --- Interface-dependent closure to RE-LOWER ---
        // A module needs RE-LOWERING (re-resolving its signatures) only when a
        // type name it references in INTERFACE position stops resolving --
        // i.e. a type this edit REMOVED or RENAMED. A type whose *shape* changed
        // (field add/retype) keeps its name, so dependents' signatures still
        // resolve it; those need only re-inference, handled below. And since a
        // dependent's TEXT is unchanged, re-lowering it never removes a name, so
        // the re-lower set is exactly one level deep -- no transitive closure.
        //
        // `removed`: type names this module defined before but not now.
        let old_types: FxHashSet<Ustr> = self
            .ws
            .struct_by_name
            .iter()
            .filter(|(_, id)| id.module == mid)
            .map(|(n, _)| *n)
            .chain(self.ws.enums_by_name.iter().filter(|(_, id)| id.module == mid).map(|(n, _)| *n))
            .chain(self.ws.newtypes_by_name.iter().filter(|(_, id)| id.module == mid).map(|(n, _)| *n))
            .collect();
        let removed: Vec<Ustr> = old_types.difference(&new_types).copied().collect();

        let mut to_relower: FxHashSet<ModuleId> = FxHashSet::default();
        for n in &removed {
            if let Some(mods) = self.dependents_iface.get(n) {
                to_relower.extend(mods.iter().copied().filter(|m| *m != mid));
            }
        }
        // Plus every module currently carrying an interface error, so a type that
        // (re)appears with this edit re-lowers its previously-dangling users and
        // clears them. User modules only: warm/builtin are never keyed in
        // `self.files`, so a warm module reaching the re-parse loop would panic.
        for m in self.ws.modules.iter() {
            if m.id != mid
                && m.id != ModuleId::builtin()
                && !self.warm_mids.contains(&m.id)
                && !m.interface_errors.is_empty()
            {
                to_relower.insert(m.id);
            }
        }

        // A type whose generic ARITY changed (but kept its name) also invalidates
        // dependents' SIGNATURES: arity is checked at LOWER time (the bare-generic
        // "requires N type argument(s)" check in `lower_type_expr`), not at infer
        // time. `removed` above only covers vanished names; a kept-name shape change
        // (field add/retype) otherwise needs only re-inference -- but arity is the
        // exception, so an arity-0 -> 1 change leaves a dependent's `fn f(b: Box)`
        // lowered against the stale arity and re-inference reports a garbled error
        // until restart. Old arity is read from `self.ws.type_generic_arity` (still
        // pre-edit here -- `lower_modules` below repopulates it); new arity from the
        // fresh AST. Same `dependents_iface` lookup the `removed` path uses.
        for it in module.items.iter() {
            let (name, new_arity) = match &it.kind {
                ItemKind::Struct(s) => (s.name.name, s.generics.len()),
                ItemKind::Enum(e) => (e.name.name, e.generics.len()),
                ItemKind::Newtype(n) => (n.name.name, n.generics.len()),
                _ => continue,
            };
            let old_arity = self.ws.type_generic_arity.get(&name).copied().unwrap_or(0);
            if old_arity != new_arity {
                if let Some(mods) = self.dependents_iface.get(&name) {
                    to_relower.extend(mods.iter().copied().filter(|m| *m != mid));
                }
            }
        }

        let closure_ms = t_closure.elapsed().as_secs_f64() * 1000.0;
        let closure_n = to_relower.len();
        let t_parse = Instant::now();

        // Re-parse each closure module from its stored text (the edited module
        // uses the provided AST); cook + lower them together with the edited
        // module so intra-closure and closure->kept references resolve.
        let mut edited = module;
        cook_one(&mut edited, &self.registry);
        let mut asts: Vec<Ast> = Vec::with_capacity(to_relower.len() + 1);
        asts.push(edited);
        for &m in &to_relower {
            let path = self.ws.module(m).file;
            let mut a = skull_parser::parse_module(&self.files[path.as_str()].text, path)?;
            cook_one(&mut a, &self.registry);
            asts.push(a);
        }
        let parse_ms = t_parse.elapsed().as_secs_f64() * 1000.0;

        self.retain_modules(|m| m != mid && !to_relower.contains(&m));
        self.ws.phase_timings.clear();

        let t_lower = Instant::now();
        let f_mids = lower_modules(&mut self.ws, &asts)?;
        let lower_ms = t_lower.elapsed().as_secs_f64() * 1000.0;
        let t_reinfer = Instant::now();

        // Names whose definition may have shifted: the edited module's interface
        // diff PLUS every name exported by a RE-LOWERED module (re-lowering may
        // have changed any of its signatures / fields -- e.g. `Wrap { inner: T }`
        // becoming `inner: Error` when `T` was removed). Body-dependents of any of
        // these must re-infer. Re-INFERRED-only modules don't propagate: their
        // interface is unchanged, so their exported definitions are stable.
        let mut reinfer_names: FxHashSet<Ustr> = changed.clone();
        for &m in &f_mids {
            let module = self.ws.module(m);
            reinfer_names.extend(exported_type_names(module));
            reinfer_names.extend(module.functions_by_name.keys().copied());
            reinfer_names.extend(module.global_by_name.keys().copied());
        }

        // Re-infer: body-dependents of `reinfer_names` (their TEXT is unchanged, so
        // body-granular reuse applies), every module with a stale inference error,
        // AND every re-lowered module (its arena shifted, so its cached inference --
        // already cleared by `retain_modules` -- can't be reused).
        let mut to_reinfer: FxHashSet<ModuleId> = reinfer_names
            .iter()
            .flat_map(|n| self.dependents.get(n).into_iter().flatten().copied())
            .collect();
        for (m, inf) in self.ws.inferences_iter() {
            if !inf.errors.is_empty() {
                to_reinfer.insert(m);
            }
        }
        to_reinfer.extend(f_mids.iter().copied());

        // `ws.modules` is append-only (`retain_modules` only nulls inference
        // slots / prunes name maps, never shrinks the vec), and `ModuleId` is its
        // dense index -- so existence is an O(1) bounds check, not an O(modules)
        // scan per dependent.
        let ids: Vec<ModuleId> = to_reinfer
            .iter()
            .copied()
            .filter(|m| m.index() < self.ws.modules.len())
            .collect();

        // Body-granular re-inference. A dependent module's TEXT is unchanged, so
        // only its bodies whose `deps` reference a changed name can have changed
        // inference -- the rest reuse their cached BodyInference (their generation
        // still matches). Take each module's cached inference and keep only the
        // reusable bodies as `prev`; `check_module_incremental` then re-runs
        // check_function for exactly the dropped (dep-touching or previously-
        // errored) bodies and reuses the rest. This is sound by the same relation
        // the module-level `dependents` index already trusts: a body with deps
        // disjoint from `changed` is no more affected than one in a module that
        // wasn't selected at all (which is never re-inferred). Globals/decorators/
        // struct-defaults always re-check inside check_module_incremental.
        let mut stat_total = 0usize;
        let mut stat_reused = 0usize;
        let prevs: Vec<(ModuleId, InferenceResult)> = ids
            .iter()
            .map(|id| {
                let cached = self
                    .ws
                    .inferences
                    .get_mut(id.index())
                    .and_then(|o| o.take())
                    .unwrap_or_default();
                let errored: FxHashSet<BodyOwner> = cached.errors.keys().copied().collect();
                let mut prev = InferenceResult::default();
                for (owner, bi) in cached.per_fn {
                    stat_total += 1;
                    if !errored.contains(&owner) && bi.deps.is_disjoint(&reinfer_names) {
                        stat_reused += 1;
                        prev.per_fn.insert(owner, bi);
                    }
                }
                (*id, prev)
            })
            .collect();
        if std::env::var("BONE_REINFER_STATS").is_ok() {
            eprintln!(
                "[reinfer] {} dependent fn-bodies: {} reused, {} re-inferred (deps touch a changed name)",
                stat_total,
                stat_reused,
                stat_total - stat_reused
            );
        }

        // check_module_incremental reads ws immutably (declared signatures from
        // the HIR, not inference results), so dependents re-check in parallel --
        // mirroring the full pipeline's `infer_modules`.
        let results: Vec<_> = prevs
            .into_par_iter()
            .map(|(id, prev)| {
                Ok::<_, CompilerError>((
                    id,
                    check_module_incremental(&self.ws, id, self.ws.module(id), prev)?,
                ))
            })
            .collect::<Result<_, _>>()?;

        for (id, inf) in results {
            self.ws.set_inference(id, inf);
        }
        let reinfer_ms = t_reinfer.elapsed().as_secs_f64() * 1000.0;

        let t_dep = Instant::now();
        self.rebuild_dependents();
        let dep_ms = t_dep.elapsed().as_secs_f64() * 1000.0;

        if stats {
            eprintln!(
                "[cascade] closure {:.3}ms ({} relower) | reparse {:.3}ms | lower {:.3}ms | reinfer {:.3}ms ({} bodies) | dep-index {:.3}ms",
                closure_ms, closure_n, parse_ms, lower_ms, reinfer_ms, ids.len(), dep_ms
            );
        }

        // `ids` is the full re-checked set and already contains `f_mids`, so it
        // covers `reinferred`. `relowered` is precisely the re-LOWERED modules
        // (`f_mids`) -- re-inferred-only dependents kept their arenas, so listing
        // them as relowered would be wrong.
        let reinferred = self.collect_reinferred(&ids);
        let relowered = self.collect_reinferred(&f_mids);

        Ok(UpdateResult {
            tier: UpdateTier::Interface,
            reinferred,
            relowered,
        })
    }

    pub fn remove_file(&mut self, path: &str) -> Result<UpdateResult, CompilerError> {
        let Some(previous) = self.files.remove(path) else {
            return Ok(UpdateResult {
                tier: UpdateTier::NoChange,
                reinferred: Vec::new(),
                relowered: Vec::new(),
            });
        };

        let pos = self
            .paths
            .iter()
            .position(|p| p == path)
            .expect("tracked file must be in path order");

        self.paths.remove(pos);

        let restore = move |s: &mut Self| {
            s.files.insert(path.to_string(), previous);
            s.paths.insert(pos, path.to_string());
        };

        self.rebuild_or_revert("removal", path, |s| s.rebuild_users(), restore)
    }

    pub fn diagnostics(&self) -> Vec<CompilerError> {
        let mut errs = body_errors(&self.ws);
        // Recovered syntax errors from each file's total parse. The compile path
        // rejects on these; the interactive path keeps the partial tree AND shows
        // them here, so a broken file gets both best-effort features and a squiggle.
        errs.extend(self.files.values().flat_map(|f| f.syntax_errors.iter().cloned()));
        errs
    }

    pub fn is_tracked(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    pub fn tracked_paths(&self) -> Vec<&str> {
        self.paths.iter().map(|s| s.as_str()).collect()
    }
    pub fn retain_modules(&mut self, keep: impl Fn(ModuleId) -> bool) {
        for (i, slot) in self.ws.inferences.iter_mut().enumerate() {
            if !keep(ModuleId::new(i as u32)) {
                *slot = None;
            }
        }

        self.ws.struct_by_name.retain(|_, id| keep(id.module));
        self.ws.enums_by_name.retain(|_, id| keep(id.module));
        self.ws.newtypes_by_name.retain(|_, id| keep(id.module));

        self.ws.functions_by_name.retain(|_, fns| {
            fns.retain(|(mid, _)| keep(*mid));
            !fns.is_empty()
        });

        self.ws.globals_by_name.clear();
        // `const_globals` accumulates the same way and must be rebuilt in lockstep:
        // it's an insert-only set (`lower_modules` never removes), so without this a
        // global flipped `const` -> mutable (or a deleted const) would stay in the
        // set and keep producing a phantom "cannot assign to const" diagnostic until
        // an LSP restart. `lower_modules` re-adds the edited/relowered modules' consts;
        // this re-adds the KEPT ones, matching `globals_by_name`.
        self.ws.const_globals.clear();

        // Skip evicted
        for module in self.ws.modules.iter().filter(|m| keep(m.id)) {
            for (_, g) in module.globals.iter() {
                self.ws.globals_by_name.insert(g.name, g.ty);
                if g.is_const {
                    self.ws.const_globals.insert(g.name);
                }
            }
        }
    }

    pub fn rebuild_users(&mut self) -> Result<UpdateResult, CompilerError> {
        // Build into a LOCAL workspace and commit to `self.ws` only once lowering
        // and inference have BOTH succeeded. `expand_and_desugar` / `lower_and_infer`
        // intern modules (bumping `module_ids`) well before `insert_module` grows the
        // `modules` vec, so a mid-build `Err` used to leave `self.ws` half-built --
        // `module_ids` running ahead of `modules` -- and the next `ws.module(mid)`
        // panicked with `len is 1 but index is N`. This bit during fast typing: a
        // broken `global x =` fails the rebuild, `rebuild_or_revert` reverts to the
        // PREVIOUS keystroke's text (also broken mid-word), so the recovery rebuild
        // failed too and the corrupted ws stuck around for every later query. Keeping
        // the last-good ws on any failure makes the session queryable after a failed
        // rebuild instead of crash-on-next-touch.
        let mut ws = Workspace::default();

        let mut expanded = self.combined_unexpanded()?;
        let registry = collect_registry(&expanded);
        expand_and_desugar(&mut ws, &mut expanded)?;

        let all_mids = lower_and_infer(&mut ws, &expanded)?;

        // Commit -- past this point nothing fails, so `self.ws` never observes a
        // partially-interned workspace.
        self.ws = ws;
        self.registry = registry;

        self.warm_mids = all_mids[..self.warm.len()].iter().copied().collect();

        let user_mids: Vec<ModuleId> = all_mids[self.warm.len()..].to_vec();
        let reinferred = self.collect_reinferred(&user_mids);
        let relowered = reinferred.clone();

        self.ws.phase_timings.clear();

        self.rebuild_dependents();

        Ok(UpdateResult {
            tier: UpdateTier::FullUser,
            reinferred,
            relowered,
        })
    }

    /// Full O(workspace) rebuild of both dep indices. For the full-rebuild paths
    /// (`Session::new`, `rebuild_users`, `rebuild_module`, `rebuild_interface`);
    /// a single-body edit uses `update_dependents_for` instead.
    pub fn rebuild_dependents(&mut self) {
        self.dependents.clear();
        self.module_dep_names.clear();
        self.dependents_iface.clear();

        for (mid, inf) in self.ws.inferences_iter() {
            if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
                continue;
            }

            let names: FxHashSet<Ustr> = inf
                .per_fn
                .values()
                .flat_map(|b| b.deps.iter().copied())
                .collect();

            for name in &names {
                self.dependents.entry(*name).or_default().insert(mid);
            }
            self.module_dep_names.insert(mid, names);
        }

        // Interface-position references: each nominal type name a module's
        // INTERFACE arena mentions (signatures / globals / fields / enum payloads).
        // Read from the lowered HIR, not inference -- a module with no inference
        // (e.g. a body-error-only module) still has interface deps.
        for module in self.ws.modules.iter() {
            let mid = module.id;
            if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
                continue;
            }
            for name in iface_dep_names(module) {
                self.dependents_iface.entry(name).or_default().insert(mid);
            }
        }
    }

    /// Incremental dep-index update for ONE re-inferred module: diff its new dep
    /// names against the cached forward index and patch only the changed reverse
    /// entries. O(mid's deps), not O(workspace) -- the body-edit fast path. Sound
    /// because a body edit changes ONLY the edited module's outgoing deps (its
    /// interface is unchanged, so no other module's deps move).
    pub fn update_dependents_for(&mut self, mid: ModuleId) {
        if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
            return;
        }

        let new = self.module_deps(mid); // owned -> releases the `ws` borrow
        let old = self.module_dep_names.remove(&mid).unwrap_or_default();

        for name in old.difference(&new) {
            if let Some(set) = self.dependents.get_mut(name) {
                set.remove(&mid);
                if set.is_empty() {
                    self.dependents.remove(name);
                }
            }
        }
        for name in new.difference(&old) {
            self.dependents.entry(*name).or_default().insert(mid);
        }

        self.module_dep_names.insert(mid, new);
    }

    pub fn module_deps(&self, mid: ModuleId) -> FxHashSet<Ustr> {
        self.ws
            .inference(mid)
            .into_iter()
            .flat_map(|inf| inf.per_fn.values())
            .flat_map(|b| b.deps.iter().copied())
            .collect()
    }

    pub fn rebuild_module(&mut self, path: &str, ast: Ast, changed: &[usize]) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        #[cfg(debug_assertions)]
        let old_iface = Some(interface_snapshot(&self.ws, mid));

        // TODO: maybe we can come up with an overload sig id that's derived from name + types and
        // not Idx<Function> for persistent ids, but maybe not needed for now
        let old_gens: FxHashMap<Idx<Function>, u64> = {
            let m = self.ws.module(mid);
            m.functions.iter()
            .filter_map(|(fid, _)| m.bodies.get(&BodyOwner::Function(fid)).map(|b| (fid, b.generation)))
            .collect()
        };

        let old_inf = self.ws.inferences.get_mut(mid.index()).and_then(|s| s.take());

        self.retain_modules(|m| m != mid);
        self.ws.phase_timings.clear();

        let mut edited = ast;
        cook_one(&mut edited, &self.registry);
        lower_modules(&mut self.ws, &[edited])?;

        let changed_owners: FxHashSet<BodyOwner> = {
            let prints = &self.files[path].item_prints;
            let module = self.ws.module(mid);

            changed.iter()
                .filter_map(|&i| prints.get(i).and_then(|p| module.body_at(p.offset)))
                .collect()
        };

        {
            let module = self.ws.module_mut(mid);

            let fids: Vec<Idx<Function>> = module.functions.iter().map(|(fid, _)| fid).collect();

            for fid in fids {
                let owner = BodyOwner::Function(fid);

                if let Some(body) = module.bodies.get_mut(&owner) {
                    let old = old_gens.get(&fid).copied().unwrap_or(0);
                    body.generation = if changed_owners.contains(&owner) { old + 1 } else { old };
                }
            }
        }

        let timer = Instant::now();
        let result = check_module_incremental(&self.ws, mid, self.ws.module(mid), old_inf.unwrap_or_default())?;
        self.ws.set_inference(mid, result);
        self.ws.log_time("infer functions", timer);

        #[cfg(debug_assertions)]
        if let Some(old) = old_iface {
            debug_assert_eq!(old, interface_snapshot(&self.ws, mid), "relower of `{path}` changed the module interface, classify mis-tiered an interface edit")
        }

        self.rebuild_dependents();

        let prints = &self.files[path].item_prints;
        let module = self.ws.module(mid);

        let reinferred: Vec<BodyId> = changed.iter()
            .filter_map(|&i| {
                prints.get(i).and_then(|p| module.body_at(p.offset)).map(|owner| (mid, owner))
            })
        .collect();

        let relowered = self.collect_reinferred(&[mid]);

        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred,
            relowered,
        })
    }

    /// Fast path for a single body edit: re-parse ONLY the edited item (no
    /// whole-file reparse), feed it to the per-body lowerer, and patch the kept
    /// state. Returns `Ok(None)` to bail to the full-parse path for ANY edit that
    /// is not a clean single-Rock-body-interior change -- so this never produces a
    /// wrong result, only ever skips work. `docs/skull-incremental-parse-plan.md`.
    fn try_incremental_body_edit(
        &mut self,
        path: &str,
        new_text: &str,
        new_hash: u64,
    ) -> Result<Option<UpdateResult>, CompilerError> {
        // The fast path carries the file's syntax errors forward UNCHANGED (it
        // re-parses only one clean body). That's wrong when the old state had a
        // recovery error -- this very edit may be FIXING it, so the stale error
        // would survive (a `... as in` -> `... as int` keystroke left a phantom
        // "unsupported type" diagnostic until LSP restart). Whenever the file
        // currently has syntax errors, take the full path, which recomputes them.
        if self.files.get(path).is_some_and(|f| !f.syntax_errors.is_empty()) {
            return Ok(None);
        }
        // --- locate (non-mutating; releases the files borrow before any change) ---
        let (es, oe, delta, idx, so, eo, line, col, old_iface, old_name, old_prints, old_macros, old_syntax_errors) = {
            let Some(old) = self.files.get(path) else { return Ok(None) };
            let (es, oe, delta) = byte_diff(&old.text, new_text);
            let Some(idx) = locate_body_edit(&old.item_prints, es, oe) else { return Ok(None) };
            let p = &old.item_prints[idx];
            (
                es, oe, delta, idx,
                p.item_span.offset_range.0, p.item_span.offset_range.1,
                p.item_span.line_range.0, p.item_span.col_range.0,
                p.iface_hash, p.name,
                old.item_prints.clone(), old.macros.clone(), old.syntax_errors.clone(),
            )
        };
        let _ = (es, oe);

        // Re-parse just this item with absolute spans (transient mini-Ast).
        let new_end = (eo as isize + delta) as usize;
        if new_end > new_text.len() || so > new_end {
            return Ok(None);
        }
        let sub = &new_text[so..new_end];
        let Ok(mut mini) = skull_parser::parse_one_item(sub, ustr(path), so, line, col) else {
            return Ok(None); // a hard tokenizer error -> let the full path surface it
        };
        // The parse is total now; a recovered (partial) item must NOT be installed
        // in place. Bail to the full reparse, which keeps the best-effort tree and
        // records the recovery error as a diagnostic.
        if !mini.recovery_errors.is_empty() {
            return Ok(None);
        }
        skull_parser::cook_one(&mut mini, &self.registry);

        // The re-parsed item must be exactly one function with the SAME name and
        // interface; otherwise the "body edit" actually moved the interface and
        // the full path must classify/route it.
        let mut mini_prints = item_prints(&mini, new_text);
        if mini_prints.len() != 1 {
            return Ok(None);
        }
        let np = mini_prints.pop().unwrap();
        if np.kind != ItemKindTag::Fn || np.name != old_name || np.iface_hash != old_iface {
            return Ok(None);
        }

        // Resolve the HIR body + spans (non-mutating). Bail if anything is off.
        let Some(&mid) = self.ws.module_ids.get(&ustr(path)) else { return Ok(None) };
        let Some(owner) = self.ws.module(mid).body_at(so) else { return Ok(None) };
        let BodyOwner::Function(fid) = owner else { return Ok(None) };
        let old_fn_span = self.ws.module(mid).functions[fid].span;
        let new_fn_span = match &mini.items[0].kind {
            ItemKind::Function(f) => f.span,
            _ => return Ok(None),
        };
        let at = old_fn_span.offset_range.1;
        let byte_delta = new_fn_span.offset_range.1 as isize - old_fn_span.offset_range.1 as isize;
        let line_delta = new_fn_span.line_range.1 as isize - old_fn_span.line_range.1 as isize;

        // --- commit: install the new prints, then relower under rebuild_or_revert
        // (on failure it restores the old file + full-rebuilds from text). ---
        let new_prints = build_shifted_prints(old_prints, idx, np, delta, line_delta);
        // Keep macro def-lines accurate: shift those declared AFTER the edited
        // item by the same delta the prints got (the edited item is a non-macro
        // fn, so a macro is wholly before or wholly after its `eo` end offset).
        let mut new_macros = old_macros;
        for md in new_macros.iter_mut() {
            if md.span.offset_range.0 >= eo {
                shift_span(&mut md.span, delta, line_delta);
            }
        }
        let previous = self.files.insert(
            path.to_string(),
            UserFile {
                text_hash: new_hash,
                item_prints: new_prints,
                text: new_text.to_string(),
                macros: new_macros,
                // The fast path only re-parses ONE clean body (it bails to the
                // full path on any recovery error), so the file's syntax errors
                // are unchanged by this localized edit.
                syntax_errors: old_syntax_errors,
            },
        );
        let restore = move |s: &mut Self| {
            if let Some(prev) = previous {
                s.files.insert(path.to_string(), prev);
            }
        };
        let result = self.rebuild_or_revert(
            "update",
            path,
            move |s| s.commit_body_relower(mid, owner, fid, mini, at, byte_delta, line_delta),
            restore,
        )?;
        Ok(Some(result))
    }

    /// The mutating half of a fast body edit: span-shift the kept bodies, relower
    /// the one edited function from `mini` (a single-item Ast), re-infer, patch
    /// deps. Mirrors `rebuild_body_incremental`'s commit half but fed the mini-Ast
    /// rather than a cooked whole-module clone.
    fn commit_body_relower(
        &mut self,
        mid: ModuleId,
        owner: BodyOwner,
        fid: Idx<Function>,
        mini: Ast,
        at: usize,
        byte_delta: isize,
        line_delta: isize,
    ) -> Result<UpdateResult, CompilerError> {
        let old_gen = self.ws.module(mid).bodies.get(&owner).map(|b| b.generation).unwrap_or(0);

        let item = &mini.items[0];
        let ItemKind::Function(f) = &item.kind else {
            comp_error!(Span::unknown(), "skull incremental: mini-Ast item is not a function");
        };
        let annotations = item.annotations.as_slice();

        let old_inf = self.ws.inferences.get_mut(mid.index()).and_then(|s| s.take());
        self.ws.phase_timings.clear();

        let mut module = std::mem::take(self.ws.module_mut(mid));

        let shift_timer = Instant::now();
        span_shift::shift_module_after_edit(&mut module, owner, at, byte_delta, line_delta);
        self.ws.log_time("shift spans", shift_timer);

        let lower_timer = Instant::now();
        let installed =
            bone_semantic::lower::relower_function_in_place(&self.ws, &mut module, &mini, f, annotations, fid)?;
        if installed {
            if let Some(b) = module.bodies.get_mut(&owner) {
                b.generation = old_gen + 1;
            }
        }
        *self.ws.module_mut(mid) = module;
        self.ws.log_time("lower functions", lower_timer);

        let infer_timer = Instant::now();
        let result = check_module_incremental(&self.ws, mid, self.ws.module(mid), old_inf.unwrap_or_default())?;
        self.ws.set_inference(mid, result);
        self.ws.log_time("infer functions", infer_timer);

        let deps_timer = Instant::now();
        self.update_dependents_for(mid);
        self.ws.log_time("rebuild dependents", deps_timer);

        let body = vec![(mid, owner)];
        Ok(UpdateResult { tier: UpdateTier::BodyOnly, reinferred: body.clone(), relowered: body })
    }

    fn rebuild_body_incremental(&mut self, path: &str, ast: Ast, changed_idx: usize) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        // New start of the edited item: equal to its old start, so bdy_at of old still
        // resolves to the right owner
        let new_start = self.files[path].item_prints[changed_idx].offset;

        let owner = match self.ws.module(mid).body_at(new_start) {
            Some(o) => o,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        // Only functions take the fast path (for now)
        // TODO: also handle globals
        let BodyOwner::Function(fid) = owner else {
            return self.rebuild_module(path, ast, &[changed_idx]);
        };

        let old_fn_span = self.ws.module(mid).functions[fid].span;
        let old_gen = self.ws.module(mid).bodies.get(&owner)
            .map(|b| b.generation).unwrap_or(0);

        // Extract the new fn span as an owned value so the borrow into `ast`
        // ends before the fallback may move `ast` into `rebuild_module`.
        let new_fn_span_opt = ast.items.iter()
            .find(|it| it.span.offset_range.0 == new_start)
            .and_then(|it| match &it.kind {
                ItemKind::Function(nf) => Some(nf.span),
                _ => None,
            });
        let new_fn_span = match new_fn_span_opt {
            Some(s) => s,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        // The threaded full-module AST is cooked in place -- no clone (the AST is
        // not persisted, so this owned copy is ours to consume). Timed as before.
        let t_clone = Instant::now();
        let mut edited = ast;
        let clone_dur = t_clone.elapsed();

        let t_cook = Instant::now();
        cook_one(&mut edited, &self.registry);
        let cook_dur = t_cook.elapsed();

        // Defensive: the edited item should survive cooking unchanged. If it
        // doesn't, `edited` is already cooked (re-cooking would double-desugar),
        // so re-parse the stored text for a fresh un-cooked module to hand to the
        // whole-module path. Effectively unreachable for a classify-confirmed
        // body edit.
        let item = match edited.items.iter().find(|it| it.span.offset_range.0 == new_start) {
            Some(it) => it,
            None => {
                let m = skull_parser::parse_module(&self.files[path].text, ustr(path))?;
                return self.rebuild_module(path, m, &[changed_idx]);
            }
        };

        let ItemKind::Function(f) = &item.kind else {
            let m = skull_parser::parse_module(&self.files[path].text, ustr(path))?;
            return self.rebuild_module(path, m, &[changed_idx]);
        };

        let annotations = item.annotations.as_slice();

        let at = old_fn_span.offset_range.1;
        let byte_delta = new_fn_span.offset_range.1 as isize - old_fn_span.offset_range.1 as isize;
        let line_delta = new_fn_span.line_range.1 as isize - old_fn_span.line_range.1 as isize;

        let old_inf = self.ws.inferences.get_mut(mid.index()).and_then(|s| s.take());
        self.ws.phase_timings.clear();
        self.ws.phase_timings.push(("clone ast", clone_dur));
        self.ws.phase_timings.push(("cook one", cook_dur));

        let mut module = std::mem::take(self.ws.module_mut(mid));

        let shift_timer = Instant::now();
        span_shift::shift_module_after_edit(&mut module, owner, at, byte_delta, line_delta);
        self.ws.log_time("shift spans", shift_timer);

        let lower_timer = Instant::now();

        let installed = bone_semantic::lower::relower_function_in_place(&self.ws, &mut module, &edited, f, annotations, fid)?;

        if installed {
            if let Some(b) = module.bodies.get_mut(&owner) {
                b.generation = old_gen + 1;
            }
        }

        *self.ws.module_mut(mid) = module;
        self.ws.log_time("lower functions", lower_timer);

        let infer_timer = Instant::now();
        let result = check_module_incremental(&self.ws, mid, self.ws.module(mid), old_inf.unwrap_or_default())?;
        self.ws.set_inference(mid, result);
        self.ws.log_time("infer functions", infer_timer);

        let deps_timer = Instant::now();
        self.update_dependents_for(mid);
        self.ws.log_time("rebuild dependents", deps_timer);

        let body = vec![(mid, owner)];

        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred: body.clone(),
            relowered: body
        })
    }

    fn rebuild_or_revert(
        &mut self,
        action: &str,
        path: &str,
        rebuild: impl FnOnce(&mut Self) -> Result<UpdateResult, CompilerError>,
        restore: impl FnOnce(&mut Self),
    ) -> Result<UpdateResult, CompilerError> {
        match rebuild(self) {
            Ok(result) => Ok(result),
            Err(e) => {
                restore(self);

                if let Err(revert_err) = self.rebuild_users() {
                    return Err(

                    CompilerError::new_no_location(format!("bone session: rebuild of previous good state failed after a failed {action} of `{path}` - session is inconsistent: {} (original {action} error: {})",
                            revert_err.message, e.message))
                    );
                }

                Err(e)
            }
        }
    }

    pub fn collect_reinferred(&self, mids: &[ModuleId]) -> Vec<BodyId> {
        let mut reinferred = Vec::new();

        for mid in mids {
            if let Some(inf) = self.ws.inference(*mid) {
                reinferred.extend(inf.per_fn.keys().map(|o| (*mid, *o)));
                reinferred.extend(inf.errors.keys().map(|o| (*mid, *o)));
            }
        }

        reinferred
    }
}

/// The macro definitions in a freshly-parsed module (Functions with `is_macro`),
/// with their parameter/return types rendered to surface text while the AST (and
/// its type arena) is still alive -- expansion will drop them from the module.
fn macro_defs(module: &Ast) -> Vec<MacroDef> {
    module
        .items
        .iter()
        .filter_map(|i| {
            let ItemKind::Function(f) = &i.kind else {
                return None;
            };
            if !f.is_macro {
                return None;
            }
            let params = f
                .params
                .iter()
                .map(|p| MacroParam {
                    name: p.name.name,
                    is_variadic: p.is_variadic,
                    ty: module.type_surface(p.ty),
                })
                .collect();
            Some(MacroDef {
                name: f.name.name,
                span: f.name.span,
                params,
                ret: f.return_ty.map(|t| module.type_surface(t)),
            })
        })
        .collect()
}

/// Does any macro defined by the old or new version appear in the changed-name
/// set? (Both versions: a removed macro is gone from `new` but its users still
/// need re-expansion; an added one isn't in `old`.) Takes the OLD names from the
/// stored `UserFile.macros` (no retained AST) and the NEW names from the freshly
/// parsed module.
fn touches_macro(old_macros: &[MacroDef], new_macros: &[MacroDef], changed: &FxHashSet<Ustr>) -> bool {
    old_macros
        .iter()
        .chain(new_macros)
        .any(|m| changed.contains(&m.name))
}

// fn module_id_of(module: &Ast, path: &str) -> ModuleId {
//     module
//         .items
//         .first()
//         .map(|i| ModuleId(i.span.file))
//         .unwrap_or_else(|| ModuleId(Ustr::from(path)))
// }

/// Minimal changed byte span between two texts: the longest common prefix `p`
/// and suffix, capped so they don't overlap. Returns `(p, old_end, delta)` where
/// the OLD text's changed region is `[p, old_end)` and `delta = new.len() -
/// old.len()`. Byte-level (UTF-8 split mid-char is harmless -- the region is only
/// used to find the containing item, and item boundaries are char-aligned).
fn byte_diff(old: &str, new: &str) -> (usize, usize, isize) {
    let o = old.as_bytes();
    let n = new.as_bytes();
    let max_pre = o.len().min(n.len());
    let mut p = 0;
    while p < max_pre && o[p] == n[p] {
        p += 1;
    }
    let max_suf = (o.len() - p).min(n.len() - p);
    let mut s = 0;
    while s < max_suf && o[o.len() - 1 - s] == n[n.len() - 1 - s] {
        s += 1;
    }
    (p, o.len() - s, new.len() as isize - old.len() as isize)
}

/// The index of the single item whose function BODY block strictly contains the
/// edit region `[es, oe)`, or `None` if the edit touches a signature, spans
/// items, or lands between them (body ranges are disjoint, so at most one matches
/// -- the extra guard is defensive).
fn locate_body_edit(prints: &[ItemPrint], es: usize, oe: usize) -> Option<usize> {
    let mut found = None;
    for (i, p) in prints.iter().enumerate() {
        if let Some((bs, be)) = p.body_range {
            if bs <= es && oe <= be {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
    }
    found
}

/// Shift a span's byte offsets + line numbers by the deltas (columns are left
/// alone -- a kept top-level item after a body edit starts on a later line but
/// the same column, mirroring `span_shift`'s reasoning).
fn shift_span(s: &mut Span, byte_delta: isize, line_delta: isize) {
    s.offset_range = (
        (s.offset_range.0 as isize + byte_delta) as usize,
        (s.offset_range.1 as isize + byte_delta) as usize,
    );
    s.line_range = (
        (s.line_range.0 as isize + line_delta) as usize,
        (s.line_range.1 as isize + line_delta) as usize,
    );
}

/// New per-item prints after a body edit: items before `idx` are unchanged, `idx`
/// is replaced with the re-parsed `np` (already absolute), and every item after
/// `idx` shifts by `(byte_delta, line_delta)` -- so the next edit still locates
/// correctly without a whole-file reparse.
fn build_shifted_prints(
    mut prints: Vec<ItemPrint>,
    idx: usize,
    np: ItemPrint,
    byte_delta: isize,
    line_delta: isize,
) -> Vec<ItemPrint> {
    for (i, p) in prints.iter_mut().enumerate() {
        if i > idx {
            p.offset = (p.offset as isize + byte_delta) as usize;
            shift_span(&mut p.item_span, byte_delta, line_delta);
            if let Some((a, b)) = p.body_range {
                p.body_range = Some((
                    (a as isize + byte_delta) as usize,
                    (b as isize + byte_delta) as usize,
                ));
            }
        }
    }
    prints[idx] = np;
    prints
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    text.hash(&mut h);
    h.finish()
}

fn hash_u64s(xs: &[u64]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    xs.hash(&mut h);
    h.finish()
}

/// The nominal type names a module's INTERFACE references. The interface arena
/// (`module.ty_arena`, `TyPos::Iface`) holds exactly the signature / global /
/// field / payload types -- body-local types live in `body.lowered_tys` -- so
/// every `App { Nominal(name) }` node in it is an interface-position reference.
/// An unresolved reference is `Error` (no name); the cascade's "re-lower every
/// module with an interface error" rule covers that case, so name erasure here
/// is safe.
/// The nominal TYPE names a module defines (struct / enum / newtype). These are
/// the names a cross-module signature resolves, so they drive the re-lower
/// cascade. Functions / globals are referenced from bodies (not type position),
/// so the re-infer set adds those separately.
fn exported_type_names(module: &Module) -> impl Iterator<Item = Ustr> + '_ {
    module
        .struct_by_name
        .keys()
        .chain(module.enums_by_name.keys())
        .chain(module.newtypes_by_name.keys())
        .copied()
}

fn iface_dep_names(module: &Module) -> FxHashSet<Ustr> {
    module
        .ty_arena
        .iter()
        .filter_map(|(_, te)| match te {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(*n),
            _ => None,
        })
        .collect()
}

fn body_errors(ws: &Workspace) -> Vec<CompilerError> {
    let mut errs: Vec<CompilerError> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten().cloned())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten().cloned()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter().cloned()))
        .collect();

    errs.sort_by(|a, b| {
        (a.location.file.as_str(), a.location.offset)
            .cmp(&(b.location.file.as_str(), b.location.offset))
    });
    errs
}

#[cfg(debug_assertions)]
/// A module's public interface, rendered so it is INVARIANT under the
/// `ty_arena` index drift that a body-only relower legitimately introduces.
///
/// Used only by the `rebuild_module` debug self-check: a body-only edit must
/// not change the module interface (else `classify` mis-tiered it). The naive
/// `format!("{:?}", &m.ty_arena ...)` is wrong here because the body-edit fast
/// path (`relower_function_in_place`) re-lowers the edited fn's SIGNATURE,
/// appending fresh `ty_arena` entries and orphaning the old ones -- so the
/// arena grows and a re-lowered fn's param/return `Idx<TypeExpr>` shifts even
/// though the signature is textually identical. Comparing raw indices + the
/// whole arena false-positives on that benign drift (it's exactly why the
/// `incremental.rs` oracle compares via `type_at`, not Debug). So we resolve
/// every interface `TyId` to its structural SHAPE and drop the arena. `VarId`s
/// are elided for the same reason: the fast path continues the var counter, so
/// a re-lowered fn's `Infer`/`Generic` carry different ids for the same type.
///
/// Functions and globals are resolved structurally (functions are the fast
/// path's only target today; globals are its planned next one -- see
/// `rebuild_body_incremental`). Structs/enums/newtypes/decorators are never
/// fast-path-relowered, so their arena indices are assigned in stable
/// traversal order and Debug-compare cleanly.
fn interface_snapshot(ws: &Workspace, mid: ModuleId) -> String {
    use std::fmt::Write;

    let m = ws.module(mid);
    let mut out = String::new();

    for (_, f) in m.functions.iter() {
        let params = f
            .param_decls
            .iter()
            .map(|p| format!("{}:{}{}", p.name, render_ty(ws, &p.ty), if p.has_default { "=?" } else { "" }))
            .collect::<Vec<_>>()
            .join(",");
        let generics = f.generics.iter().map(|g| render_ty(ws, g)).collect::<Vec<_>>().join(",");
        let ret = f.return_ty.as_ref().map(|t| render_ty(ws, t)).unwrap_or_default();
        let _ = write!(
            out,
            "fn {} co={} var={} gen=[{generics}] params=[{params}] req={} ret={ret} lua={:?} extern={} anns={:?} ffi={:?}\n",
            f.name, f.is_coroutine, f.is_variadic, f.num_required, f.lua_body, f.is_extern, f.annotations, f.ffi_lib,
        );
    }

    for (_, g) in m.globals.iter() {
        let _ = write!(out, "global {}:{}\n", g.name, render_ty(ws, &g.ty));
    }

    let _ = write!(out, "{:?}", (&m.structs, &m.enums, &m.newtypes, &m.decorators));
    out
}

/// Resolve a `TyId` to a canonical, `ty_arena`-index-independent shape string.
fn render_ty(ws: &Workspace, ty: &bone_semantic::hir::TyId) -> String {
    use bone_semantic::hir::TyId;
    match ty {
        TyId::Static(mid, idx) => render_ty_node(ws, *mid, *idx),
        // Body-local types never appear in interface position; render defensively.
        TyId::Local(idx) => format!("local#{idx:?}"),
    }
}

fn render_ty_node(ws: &Workspace, mid: ModuleId, idx: Idx<bone_semantic::hir::TypeExpr>) -> String {
    use bone_semantic::hir::TypeExpr;

    let render_range = |range: IdxRange<TypeExpr>| {
        range.map(|i| render_ty_node(ws, mid, i)).collect::<Vec<_>>().join(",")
    };

    match &ws.module(mid).ty_arena[idx] {
        TypeExpr::Builtin(b) => format!("{b:?}"),
        // VarId elided -- the fast path continues the var counter, so the same
        // type carries different ids when re-lowered vs freshly lowered.
        TypeExpr::Generic(_, n) => format!("Generic({n})"),
        TypeExpr::Infer(_, n) => format!("Infer({n})"),
        TypeExpr::Nullable(i) => format!("?{}", render_ty_node(ws, mid, *i)),
        TypeExpr::App { ctor, params } => format!("App({ctor:?},[{}])", render_range(*params)),
        TypeExpr::Function { params, ret } => {
            format!("Fn([{}]=>{})", render_range(*params), render_ty_node(ws, mid, *ret))
        }
        TypeExpr::Tuple { elements } => format!("Tuple([{}])", render_range(*elements)),
    }
}
