//! Dispatch contextual completion and rank candidates for general completion.

use super::*;
use crate::cursor_context::Record;
use crate::modules_scope::ResolvedNames;
use crate::query::{LocalScope, VisibleBinding};
use crate::reachability::ReachabilityIndex;

struct Request<'a> {
    path: &'a str,
    src: &'a str,
    offset: usize,
    prefix_raw: &'a str,
    prefix: String,
    next_is_open_paren: bool,
    munges: std::cell::OnceCell<[String; 2]>,
}

impl Request<'_> {
    fn munges(&self) -> &[String; 2] {
        self.munges
            .get_or_init(|| completion_munges(self.src, self.offset))
    }

    fn speculate<T>(
        &self,
        session: &mut Session,
        query: impl Fn(&Workspace) -> Option<T>,
    ) -> Option<T> {
        self.munges()
            .iter()
            .find_map(|text| analyze_variant(session, self.path, text, |ws, _| query(ws)).flatten())
    }
}

struct Caches<'a> {
    reach: &'a crate::reachability::ReachabilityIndex,
    freq: &'a HashMap<Ustr, u32>,
    call_through: &'a CallThroughIndex,
    scopes: &'a mut HashMap<Ustr, CompletionScope>,
}

pub(super) struct CompletionScope {
    names: ResolvedNames,
    visible: Vec<VisibleBinding>,
    call_through: std::sync::OnceLock<ScopedCallThroughIndex>,
}

impl CompletionScope {
    fn build(ws: &Workspace, frag: &bone_core::modules::FragmentScope) -> Self {
        let mut names = ResolvedNames::build(ws, frag);
        let mut global_def_files = HashMap::new();
        for module in &ws.modules {
            for key in module.global_by_name.keys() {
                global_def_files.entry(*key).or_insert(module.file);
            }
        }
        let mut tests = FxHashMap::default();
        let visible = std::mem::take(&mut names.ordinary)
            .into_iter()
            .filter_map(|(name, key)| {
                crate::query::resolve_visible_binding(
                    ws,
                    frag,
                    name,
                    key,
                    global_def_files.get(&key).copied(),
                    &mut tests,
                )
            })
            .collect();
        Self {
            names,
            visible,
            call_through: Default::default(),
        }
    }
}

impl BoneWorkspace {
    pub fn completion(&mut self, path: &str, pos: Position) -> Vec<CompletionItem> {
        self.completion_with_trigger(path, pos, false)
    }

    pub fn completion_with_trigger(
        &mut self,
        path: &str,
        pos: Position,
        space_triggered: bool,
    ) -> Vec<CompletionItem> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.projects.get_mut(&Self::root_for(path)) else {
            return Vec::new();
        };
        let BoneProject {
            fallbacks,
            session,
            texts,
            reach,
            freq,
            call_through,
            completion_scopes,
            ..
        } = proj;
        let Some(src) = texts.get(path) else {
            return Vec::new();
        };
        let src = src.as_str();
        let offset = byte_offset(src, pos);
        let mut items = lexical_gate(src, offset, space_triggered).unwrap_or_else(|| {
            let prefix_raw = ident_prefix(src, offset);
            let rq = Request {
                path,
                src,
                offset,
                prefix_raw,
                prefix: prefix_raw.to_lowercase(),
                munges: Default::default(),
                next_is_open_paren: src.get(offset..).is_some_and(|s| s.starts_with('(')),
            };
            let cx = classify(src, offset);
            let ws = session.workspace();
            let reach =
                &*reach.get_or_insert_with(|| crate::reachability::ReachabilityIndex::build(ws));
            if let Some(items) = contextual_completions(session, reach, &rq, &cx) {
                return items;
            }
            let ws = session.workspace();
            let freq = &*freq.get_or_insert_with(|| build_freq_index(ws));
            let call_through = &*call_through.get_or_insert_with(|| CallThroughIndex::build(ws));
            let caches = Caches {
                reach,
                freq,
                call_through,
                scopes: completion_scopes,
            };
            general_completions(session, fallbacks, caches, &rq, &cx)
        });
        reconcile_suffix(&mut items, member_tail(src, offset));
        items
    }
}

fn lexical_gate(src: &str, offset: usize, space_triggered: bool) -> Option<Vec<CompletionItem>> {
    // A space trigger on a blank line prefix is the editor's `<CR>` auto-indent.
    if space_triggered && line_prefix_is_blank(src, offset) {
        return Some(Vec::new());
    }
    match rock_tokenizer::region_at(src, offset) {
        rock_tokenizer::LexRegion::Comment => {
            return Some(magic_comment_completions(src, offset).unwrap_or_default());
        }
        rock_tokenizer::LexRegion::StringText => return Some(Vec::new()),
        rock_tokenizer::LexRegion::Code => {}
    }
    // A digit starts a numeric literal; don't match it against symbols
    // (`0` -> `remap_to_01`).
    if ident_prefix(src, offset)
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_digit)
    {
        return Some(Vec::new());
    }
    None
}

fn contextual_completions(
    session: &mut Session,
    reach: &crate::reachability::ReachabilityIndex,
    rq: &Request,
    cx: &CompletionCx,
) -> Option<Vec<CompletionItem>> {
    let file = Ustr::from(rq.path);
    let pfx = rq.prefix.as_bytes();
    for target in cx.contexts() {
        let (candidates, strip_call, pattern) = match target {
            Context::Member { receiver, path } => (
                member_candidates_at(session, reach, rq, cx, receiver, path),
                open_paren_follows(rq.src, rq.offset),
                false,
            ),
            Context::FieldRef {
                callee,
                index,
                receiver,
            } => {
                let lead_dot = leading_dot(rq.src, rq.offset);
                let ws = session.workspace();
                let Some(frag) = crate::modules_scope::fragment(ws, file) else {
                    continue;
                };
                if !lead_dot
                    && callee_slot_is_field_of_in_scope(ws, frag, callee, index) == Some(false)
                {
                    continue;
                }
                let candidates = rq.speculate(session, |ws| {
                    field_ref_candidates(
                        ws, rq.path, rq.src, rq.offset, callee, index, &rq.prefix, lead_dot,
                        receiver,
                    )
                });
                (candidates, false, false)
            }
            Context::Record(record) => {
                let written = match record {
                    Record::Struct { brace_open, .. } => {
                        rq.src.get(*brace_open + 1..rq.offset).unwrap_or("")
                    }
                    Record::Variant { .. } => "",
                };
                let candidates = rq.speculate(session, |ws| {
                    let mid = user_module_for_path(ws, rq.path)?;
                    record_candidates(ws, ws.module(mid).file, pfx, record, written)
                });
                (candidates, false, false)
            }
            Context::ModulePath => (
                module_path_candidates(
                    session.workspace(),
                    rq.path,
                    rq.src,
                    rq.offset - rq.prefix_raw.len(),
                    &rq.prefix,
                ),
                false,
                false,
            ),
            Context::Enum(name) => {
                let candidates = rq.speculate(session, |ws| {
                    let mid = user_module_for_path(ws, rq.path)?;
                    variant_candidates(ws, ws.module(mid).file, name, &rq.prefix)
                });
                (
                    Some(candidates.unwrap_or_default()),
                    rq.next_is_open_paren,
                    cx.pattern_scrutinee.is_some(),
                )
            }
            Context::Decorator => {
                let ws = session.workspace();
                let frag = crate::modules_scope::fragment(ws, file)?;
                (
                    Some(decorator_candidates(ws, frag, &rq.prefix)),
                    false,
                    false,
                )
            }
        };
        if let Some(candidates) = candidates {
            let mut items: Vec<_> = candidates
                .into_iter()
                .map(|cand| materialize_cand(cand, session.workspace(), file, strip_call, pattern))
                .collect();
            if rq.next_is_open_paren && matches!(target, Context::Member { .. }) {
                strip_call_template(&mut items);
            }
            items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
            return Some(items);
        }
    }
    None
}

fn member_candidates_at(
    session: &mut Session,
    reach: &crate::reachability::ReachabilityIndex,
    rq: &Request,
    cx: &CompletionCx,
    receiver: usize,
    path: Option<&[String]>,
) -> Option<Vec<Cand>> {
    let arg_slot = cx
        .arg_call
        .as_ref()
        .map(|(callee, idx, _)| (callee.as_str(), *idx as usize));
    let mut resolved = path.is_some();
    for named in [false, true] {
        if named && path.is_none() {
            break;
        }
        for text in rq.munges() {
            let candidates = analyze_variant(session, rq.path, text, |ws, live| {
                let mid = user_module_for_path(ws, rq.path)?;
                let fallback_reach;
                let reach = if live {
                    reach
                } else {
                    fallback_reach = crate::reachability::ReachabilityIndex::build(ws);
                    &fallback_reach
                };
                let members = if named {
                    crate::query::members_of_name_path(ws, mid, rq.offset, path?)
                } else {
                    crate::query::members_of_receiver_expr(ws, mid, receiver)
                }?;
                Some(member_candidates(
                    ws,
                    mid,
                    rq.offset,
                    members,
                    &rq.prefix,
                    arg_slot,
                    cx.anchor,
                    reach,
                    cx.bool_chain,
                ))
            })
            .flatten();
            if let Some(candidates) = candidates {
                if !candidates.is_empty() {
                    return Some(candidates);
                }
                resolved = true;
            }
        }
    }
    resolved.then(Vec::new)
}

fn module_path_candidates(
    ws: &Workspace,
    path: &str,
    src: &str,
    at: usize,
    prefix: &str,
) -> Option<Vec<Cand>> {
    let frag = crate::modules_scope::fragment(ws, Ustr::from(path))?;
    let (from_root, segments) = qualified_prefix_before(src, at)?;
    let members = crate::modules_scope::module_members(ws, frag, from_root, &segments)?;
    let pfx = prefix.as_bytes();
    Some(
        members
            .into_iter()
            .filter(|(name, _)| admit(name.as_bytes(), pfx))
            .enumerate()
            .map(|(i, (name, member))| {
                let kind = match member {
                    crate::modules_scope::Member::Module => CompletionItemKind::MODULE,
                    crate::modules_scope::Member::Def(key) => {
                        if ws.functions_by_name.contains_key(&key) {
                            CompletionItemKind::FUNCTION
                        } else if ws.nominals.struct_id(key).is_some() {
                            CompletionItemKind::STRUCT
                        } else if ws.nominals.enum_id(key).is_some() {
                            CompletionItemKind::ENUM
                        } else if ws.nominals.newtype_id(key).is_some() {
                            CompletionItemKind::STRUCT
                        } else {
                            CompletionItemKind::VARIABLE
                        }
                    }
                };
                Cand::new(
                    closed_set_rank(name.as_str(), pfx, 0, i),
                    CandName::Sym(name),
                    CandShape::Label { kind },
                )
            })
            .collect(),
    )
}

struct GeneralCx<'w> {
    ws: &'w Workspace,
    mid: ModuleId,
    inf: &'w InferenceResult,
    frag: &'w bone_core::modules::FragmentScope,
    enclosing_fn: Option<Ustr>,
    locals: LocalScope<'w>,
    pattern_pos: bool,
}

fn general_completions(
    session: &mut Session,
    fallbacks: &FallbackReports,
    caches: Caches,
    rq: &Request,
    cx: &CompletionCx,
) -> Vec<CompletionItem> {
    let pattern_pos = cx.pattern_scrutinee.is_some();
    let pattern_enum: Option<String> = cx
        .pattern_scrutinee
        .and_then(|a| pattern_scrutinee_enum(session, rq.path, rq.src, rq.offset, a));
    let fallback_ws = recover_missing_module(session, fallbacks, rq.path, "completion", || {
        if pattern_pos {
            // Keep the incomplete match arm and its scrutinee in the recovered AST.
            let (head, tail) = rq.src.split_at(rq.offset.min(rq.src.len()));
            format!("{head} => {{}}{tail}")
        } else {
            rq.src.to_owned()
        }
    });
    let ws = fallback_ws.as_ref().unwrap_or(session.workspace());
    let Some(mid) = user_module_for_path(ws, rq.path) else {
        return Vec::new();
    };
    let Some(frag) = crate::modules_scope::fragment(ws, ws.module(mid).file) else {
        return Vec::new();
    };
    let Some(inf) = ws.inference(mid) else {
        return Vec::new();
    };
    let module = ws.module(mid);
    let function = match module.body_at(rq.offset) {
        Some(bone_semantic::hir::BodyOwner::Function(fid)) => Some(&module.functions[fid]),
        _ => None,
    };
    let enclosing_fn = function.map(|f| f.name);
    let g = GeneralCx {
        ws,
        mid,
        inf,
        frag,
        enclosing_fn,
        locals: LocalScope::at(ws, mid, Some(inf), rq.offset),
        pattern_pos,
    };

    let fallback_scope = fallback_ws
        .is_some()
        .then(|| CompletionScope::build(ws, frag));
    let scope = fallback_scope.as_ref().unwrap_or_else(|| {
        caches
            .scopes
            .entry(ws.module(mid).file)
            .or_insert_with(|| CompletionScope::build(ws, frag))
    });
    let mut cands = crate::query::completion_with_visible_bindings(
        ws,
        ws.module(mid).file,
        frag,
        &g.locals,
        &rq.prefix,
        cx.expr_position || pattern_pos,
        function.is_some_and(|f| f.is_coroutine),
        &scope.visible,
    );
    if cx.expr_position {
        push_macro_candidates(session, rq, &mut cands);
    }
    let import_edits =
        push_import_candidates(&g, session, rq, cx.expr_position || pattern_pos, &mut cands);

    let mut expected = cx.expected(ws, mid, inf, rq.offset);
    let has_expected = expected.is_some();
    let mut general: Vec<Cand> = {
        let mut ranker = GeneralRanker::new(
            &g,
            rq,
            cx,
            pattern_enum.as_deref(),
            expected.as_mut(),
            caches.freq,
        );
        cands.into_iter().map(|c| ranker.rank(c)).collect()
    };
    if general.len() > MAX_COMPLETION_ITEMS {
        general.select_nth_unstable_by(MAX_COMPLETION_ITEMS, cand_cmp);
        general.truncate(MAX_COMPLETION_ITEMS);
    }
    let mut sink = Sink::new();
    sink.cands = general;
    push_entity_synth(&g, rq, cx, &mut sink);
    push_named_args(&g, rq, cx, &mut sink);
    push_coroutine_synths(&g, rq, cx, &mut sink);

    let fallback_indexes = fallback_ws
        .is_some()
        .then(|| (ReachabilityIndex::build(ws), CallThroughIndex::build(ws)));
    let (reach, call_through) = fallback_indexes
        .as_ref()
        .map_or((caches.reach, caches.call_through), |(reach, calls)| {
            (reach, calls)
        });
    if let Some(exp) = expected.as_ref() {
        if !exp.rendered.is_empty() && !pattern_pos {
            push_reach_paths(
                &g,
                reach,
                call_through,
                &scope.names,
                rq,
                &exp.rendered,
                &mut sink,
            );
        }
    }

    // Skipping call-through at the cap can hide a better local-seeded call;
    // narrowing the prefix surfaces it.
    if cx.expr_position && !pattern_pos && rq.prefix.len() >= 2 && sink.len() < MAX_COMPLETION_ITEMS
    {
        let scoped = scope.call_through.get_or_init(|| {
            ScopedCallThroughIndex::build(ws, frag, &scope.names, call_through, reach)
        });
        call_through_param_completions(
            ws,
            &g.locals,
            &rq.prefix,
            call_through,
            frag,
            &scope.names,
            scoped,
            reach,
            !has_expected,
            expected.as_mut(),
            enclosing_fn.as_deref(),
            &mut sink,
        );
    }
    finish(&g, sink, rq, &import_edits)
}

fn push_macro_candidates(
    session: &Session,
    rq: &Request,
    cands: &mut Vec<crate::query::CompletionEntry>,
) {
    let pfx = rq.prefix.as_bytes();
    let show_private = rq.prefix.starts_with('_');
    let mut already: std::collections::HashSet<Ustr> = cands.iter().map(|c| c.name).collect();
    for (def_file, md) in session.all_macro_defs() {
        let name = md.name;
        if name.as_str().starts_with('_') && !show_private {
            continue;
        }
        if !admit(name.as_str().as_bytes(), pfx) {
            continue;
        }
        if !already.insert(name) {
            continue;
        }
        let params = crate::query::rendered_macro_params(md);
        let ret = md
            .ret
            .as_ref()
            .map_or_else(String::new, |r| format!(" -> {r}"));
        cands.push(crate::query::CompletionEntry {
            detail: Some(format!("macro {name}({}){ret}", params.join(", "))),
            test_scoped: crate::query::is_test_path(def_file.as_str())
                || name.as_str().starts_with("test_"),
            ..crate::query::CompletionEntry::new(
                name,
                crate::query::CompletionSymbol::Macro {
                    takes_params: !md.params.is_empty(),
                },
                def_file,
            )
        });
    }
}

fn push_import_candidates(
    g: &GeneralCx,
    session: &Session,
    rq: &Request,
    expr_position: bool,
    cands: &mut Vec<crate::query::CompletionEntry>,
) -> HashMap<String, TextEdit> {
    let mut import_edits = HashMap::new();
    if rq.prefix.is_empty() {
        return import_edits;
    }
    let frag = g.frag;
    let ws = g.ws;
    let show_private = rq.prefix.starts_with('_');
    let pfx = rq.prefix.as_bytes();
    let admit_name = |name: &str| {
        (!name.starts_with('_') || show_private)
            && (admit(name.as_bytes(), pfx)
                || (expr_position && name.starts_with(|c: char| c.is_ascii_uppercase())))
    };
    let contribution = session.files.get(rq.path).map(|uf| &uf.contribution);
    let mut offered: std::collections::HashSet<Ustr> = cands.iter().map(|c| c.name).collect();
    let mut entries = Vec::new();
    let mut test_path_cache: FxHashMap<Ustr, bool> = FxHashMap::default();
    for cand in crate::modules_scope::import_candidates(ws, frag, admit_name) {
        crate::query::key_entries_in_scope(
            ws,
            frag,
            cand.name,
            cand.key,
            expr_position,
            pfx,
            show_private,
            &mut entries,
            &mut test_path_cache,
        );
        for entry in entries.drain(..) {
            if !offered.insert(entry.name) {
                continue;
            }
            let (at, new_text) = match contribution {
                Some(c) => crate::modules_scope::use_insertion(c, rq.src, &cand.path),
                None => (0, format!("use {};\n", cand.path)),
            };
            let at = offset_to_position(rq.src, at);
            import_edits.insert(
                entry.name.to_string(),
                TextEdit {
                    range: Range::new(at, at),
                    new_text,
                },
            );
            cands.push(entry);
        }
    }
    import_edits
}

struct GeneralRanker<'a, 'w> {
    g: &'a GeneralCx<'w>,
    path: &'a str,
    q: &'a [u8],
    q_raw: &'a [u8],
    pattern_enum: Option<&'a str>,
    for_in: bool,
    for_in_oracle: Option<ForInOracle<'w>>,
    after_yield: bool,
    demote_tests: bool,
    expected: Option<&'a mut ExpectedArg<'w>>,
    has_expected: bool,
    freq: &'a HashMap<Ustr, u32>,
    root: String,
    prox_memo: HashMap<Ustr, u8>,
}

impl<'a, 'w> GeneralRanker<'a, 'w> {
    fn new(
        g: &'a GeneralCx<'w>,
        rq: &'a Request,
        cx: &CompletionCx,
        pattern_enum: Option<&'a str>,
        expected: Option<&'a mut ExpectedArg<'w>>,
        freq: &'a HashMap<Ustr, u32>,
    ) -> Self {
        let ws = g.ws;
        let for_in_oracle = cx
            .for_in
            .then(|| ForInOracle::new(ws, g.mid, g.inf, rq.offset))
            .flatten();
        GeneralRanker {
            g,
            path: rq.path,
            q: rq.prefix.as_bytes(),
            q_raw: rq.prefix_raw.as_bytes(),
            pattern_enum,
            for_in: cx.for_in,
            for_in_oracle,
            after_yield: cx.after_yield,
            demote_tests: !crate::query::is_test_path(rq.path),
            has_expected: expected.is_some(),
            expected,
            freq,
            root: BoneWorkspace::root_for(rq.path)
                .to_string_lossy()
                .into_owned(),
            prox_memo: HashMap::new(),
        }
    }

    fn prox(&mut self, def_file: Ustr) -> u8 {
        let (path, root) = (self.path, &self.root);
        *self.prox_memo.entry(def_file).or_insert_with(|| {
            let f = def_file.as_str();
            if f == path {
                0
            } else if rock_stdlib::is_stdlib_module_file(f) || f.starts_with('<') {
                3
            } else if f.starts_with(root.as_str()) {
                1
            } else {
                2
            }
        })
    }

    fn rank(&mut self, c: crate::query::CompletionEntry) -> Cand {
        let ws = self.g.ws;
        let (q, q_raw) = (self.q, self.q_raw);
        // Structs share bucket 2 with functions: below them, the empty-prefix cap
        // would truncate every type away.
        let kind = match c.kind() {
            _ if self.demote_tests && c.test_scoped => 4,
            SymCat::Local | SymCat::Param => 0,
            SymCat::Function
                if self.after_yield
                    && fn_is_coroutine_in_scope(ws, c.name.as_str(), self.g.frag) =>
            {
                0
            }
            SymCat::Global => 1,
            SymCat::Variant => 3,
            _ => 2,
        };
        let type_rank = self.type_rank(&c);
        let method = u8::from(c.kind() == SymCat::Function);
        let name = c.name.as_str();
        let case_rank = if !self.has_expected && self.pattern_enum.is_none() && !self.for_in {
            let case_target = if c.kind() == SymCat::Variant {
                name.rsplit("::").next().unwrap_or(name)
            } else {
                name
            };
            case_mismatch_rank(case_target.as_bytes(), q_raw)
        } else {
            0
        };
        let (score, lead) = if c.kind() == SymCat::Variant {
            let vname = name.rsplit("::").next().unwrap_or(name);
            let s = fuzzy_score(vname.as_bytes(), q).max(fuzzy_score(name.as_bytes(), q));
            let l = leading_run_len(vname.as_bytes(), q).max(leading_run_len(name.as_bytes(), q));
            (s, l)
        } else {
            (
                fuzzy_score(name.as_bytes(), q),
                leading_run_len(name.as_bytes(), q),
            )
        };
        let score = score + (lead as i32) * 100;
        let (exact, band) = match_band_exact(name, q, q_raw);
        let freq_name = c.key();
        let key = RankKey {
            exact,
            type_rank,
            band,
            case: case_rank,
            kind,
            dist: 0,
            method,
            fine: RankKey::fine(score),
            prox: self.prox(c.def_file),
            freq: match c.kind() {
                SymCat::Local | SymCat::Param => 0,
                _ => freq_bucket(self.freq.get(&freq_name).copied().unwrap_or(0)),
            },
            rec: c.rec(),
            len: RankKey::len_key(score, name.len()),
        };
        Cand::new(key, CandName::Sym(c.name), CandShape::Entry(c))
    }

    fn type_rank(&mut self, c: &crate::query::CompletionEntry) -> u8 {
        use crate::query::CompletionSymbol;
        let ws = self.g.ws;
        if let Some(expected) = self.pattern_enum {
            return match c.symbol {
                CompletionSymbol::Variant { enum_key, .. } if enum_key.as_str() == expected => 0,
                _ => 2,
            };
        }
        if self.for_in {
            let Some(oracle) = self.for_in_oracle.as_mut() else {
                return 2;
            };
            return match c.symbol {
                CompletionSymbol::Variable { ty: Some(ty), .. } if oracle.iterable(ty) => 0,
                CompletionSymbol::Function { key, .. }
                    if fn_unique_return_ty_in_scope(ws, key.as_str(), self.g.frag)
                        .is_some_and(|ty| oracle.iterable(ty)) =>
                {
                    1
                }
                _ => 2,
            };
        }
        let Some(expected) = self.expected.as_deref_mut() else {
            return 0;
        };
        match c.symbol {
            CompletionSymbol::Variable { ty: Some(ty), .. } if expected.accepts(ty) => 0,
            CompletionSymbol::Variant { enum_key, .. }
                if expected.expects_nominal(enum_key.as_str()) =>
            {
                0
            }
            CompletionSymbol::Function { key, .. }
                if self.g.enclosing_fn.as_deref() != Some(key.as_str())
                    && expected.accepts_fn_returning(ws, key.as_str()) =>
            {
                1
            }
            _ => 2,
        }
    }
}

fn freq_bucket(n: u32) -> u8 {
    if n >= FREQ_HOT {
        0
    } else if n >= FREQ_WARM {
        1
    } else {
        2
    }
}

fn push_entity_synth(g: &GeneralCx, rq: &Request, cx: &CompletionCx, sink: &mut Sink) {
    let Some((pn, fn_name)) = cx.param_type.as_ref() else {
        return;
    };
    if !is_entity_param_name(pn) {
        return;
    }
    let payload = fn_prefix_struct(g.ws, g.frag, fn_name);
    let label = match payload {
        Some(p) => format!("Entity<{p}>"),
        None => "Entity<>".to_string(),
    };
    if admit(label.as_bytes(), rq.prefix.as_bytes()) {
        let key = RankKey {
            len: RankKey::len_key(9999, label.len()),
            ..Default::default()
        };
        sink.push(Cand::new(
            key,
            CandName::Owned(label),
            CandShape::EntitySynth { payload },
        ));
    }
}

fn push_named_args(g: &GeneralCx, rq: &Request, cx: &CompletionCx, sink: &mut Sink) {
    let Some(nargs) = cx.named_arg.as_ref() else {
        return;
    };
    let spread_covered: std::collections::HashSet<Ustr> = nargs
        .spread_probes
        .iter()
        .flat_map(|&p| crate::query::struct_field_names_at(g.ws, g.mid, g.inf, p))
        .collect();
    let q = rq.prefix.as_bytes();
    for (i, (pname, pty)) in callee_param_names_in_scope(g.ws, &nargs.callee, g.frag)
        .into_iter()
        .enumerate()
    {
        if i < nargs.positional
            || nargs.named.iter().any(|n| n.as_str() == pname.as_str())
            || spread_covered.contains(&pname)
        {
            continue;
        }
        if !q.is_empty() && !admit(pname.as_str().as_bytes(), q) {
            continue;
        }
        let score = fuzzy_score(pname.as_str().as_bytes(), q);
        let (ex, bd) = match_band_exact(pname.as_str(), q, rq.prefix_raw.as_bytes());
        let key = RankKey {
            exact: ex,
            band: bd,
            fine: RankKey::fine(score),
            len: RankKey::len_key(score, pname.as_str().len()),
            ..Default::default()
        };
        sink.push(Cand::new(
            key,
            CandName::Sym(pname),
            CandShape::NamedArg { pname, pty },
        ));
    }
}

fn push_coroutine_synths(g: &GeneralCx, rq: &Request, cx: &CompletionCx, sink: &mut Sink) {
    let has_wait = nominal_exists(g.ws, g.frag, "Wait");
    let q = rq.prefix.as_bytes();
    if cx.coroutine_generic {
        if has_wait && admit(b"Wait", q) {
            sink.retain_not_named("Wait");
            let key = RankKey {
                len: RankKey::len_key(9999, "Wait".len()),
                ..Default::default()
            };
            sink.push(Cand::new(
                key,
                CandName::Owned("Wait".to_string()),
                CandShape::Wait,
            ));
        }
    } else if !cx.expr_position && !q.is_empty() && admit(b"Coroutine", q) {
        sink.retain_not_named("Coroutine");
        if has_wait {
            let key = RankKey {
                len: RankKey::len_key(9999, "Coroutine<Wait>".len()),
                ..Default::default()
            };
            sink.push(Cand::new(
                key,
                CandName::Owned("Coroutine<Wait>".to_string()),
                CandShape::Coroutine { with_wait: true },
            ));
        }
        let key = RankKey {
            fine: RankKey::fine(9998),
            len: RankKey::len_key(9998, "Coroutine<>".len()),
            ..Default::default()
        };
        sink.push(Cand::new(
            key,
            CandName::Owned("Coroutine<>".to_string()),
            CandShape::Coroutine { with_wait: false },
        ));
    }
    if q.is_empty() && co_fn_return_slot(rq.src.as_bytes(), rq.offset) {
        let label = if has_wait {
            "-> Coroutine<Wait>"
        } else {
            "-> Coroutine<>"
        };
        let key = RankKey {
            len: RankKey::len_key(9999, label.len()),
            ..Default::default()
        };
        sink.push(Cand::new(
            key,
            CandName::Owned(label.to_string()),
            CandShape::CoReturn { has_wait },
        ));
    }
}

fn push_reach_paths(
    g: &GeneralCx,
    reach: &ReachabilityIndex,
    index: &CallThroughIndex,
    names: &ResolvedNames,
    rq: &Request,
    expected_types: &[String],
    sink: &mut Sink,
) {
    let (ws, frag) = (g.ws, g.frag);
    let pfx = rq.prefix.as_bytes();
    let show_private = rq.prefix.starts_with('_');
    let mut globals: Vec<_> = index
        .globals(names)
        .filter(|root| {
            (show_private || !root.name.as_str().starts_with('_'))
                && !g
                    .locals
                    .bindings
                    .iter()
                    .any(|local| local.name == root.name)
        })
        .collect();
    globals.sort_by_key(|root| root.name.as_str());
    let roots = local_roots(ws, &g.locals)
        .map(|root| (root, 0))
        .chain(globals.into_iter().map(|root| (root, 1)));
    for (root, root_kind) in roots {
        let res = reach.paths_to(
            root.ty.as_str(),
            expected_types,
            REACH_CAP,
            Some((ws, frag)),
        );
        for p in res.paths {
            if g.enclosing_fn
                .is_some_and(|f| reach_suffix_calls(&p.suffix, &f))
            {
                continue;
            }
            let Some(key) = root.path_text(&p, &names.callables) else {
                continue;
            };
            if !pfx.is_empty() && !clean_match(key.as_bytes(), pfx) {
                continue;
            }
            if !sink.admit(&key) {
                continue;
            }
            let final_ty =
                crate::modules_scope::display_type_with_names(ws, &p.final_ty, &names.types);
            let (type_rank, kind) = if p.is_method { (1, 2) } else { (0, root_kind) };
            let rk = arg_rank_key(type_rank, kind, &key, pfx, p.dist, u8::from(p.is_method), 0);
            sink.push(Cand::new(
                rk,
                CandName::Owned(key),
                CandShape::Reach {
                    final_ty,
                    is_method: p.is_method,
                    takes_args: p.takes_args,
                },
            ));
        }
    }
}

fn finish(
    g: &GeneralCx,
    mut sink: Sink,
    rq: &Request,
    import_edits: &HashMap<String, TextEdit>,
) -> Vec<CompletionItem> {
    let ws = g.ws;
    let fragment_file = ws.module(g.mid).file;
    sink.cands.sort_by(cand_cmp);
    sink.cands.truncate(MAX_COMPLETION_ITEMS);
    let mut items: Vec<CompletionItem> = sink
        .cands
        .into_iter()
        .map(|c| materialize_cand(c, ws, fragment_file, rq.next_is_open_paren, g.pattern_pos))
        .collect();
    if !import_edits.is_empty() {
        for item in &mut items {
            if let Some(edit) = import_edits.get(&item.label) {
                item.additional_text_edits = Some(vec![edit.clone()]);
            }
        }
    }
    items
}
