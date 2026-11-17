//! Dispatch contextual completion and rank candidates for general completion.

use super::*;

/// The request-wide facts every source reads.
struct Request<'a> {
    path: &'a str,
    src: &'a str,
    offset: usize,
    /// The identifier under the cursor as typed.
    prefix_raw: &'a str,
    /// `prefix_raw` lowercased: the match and rank query.
    prefix: String,
    /// Reuse existing call parentheses when inserting a completion.
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
            .find_map(|text| analyze_variant(session, self.path, text, &query).flatten())
    }
}

/// Project indexes shared across completion requests until the interface changes.
struct Caches<'a> {
    reach: &'a crate::reachability::ReachabilityIndex,
    freq: &'a HashMap<Ustr, u32>,
    call_through: &'a CallThroughIndex,
    scoped_call_through: &'a mut HashMap<Ustr, ScopedCallThroughIndex>,
    visible_bindings: &'a mut HashMap<Ustr, Vec<crate::query::VisibleBinding>>,
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
            scoped_call_through,
            visible_bindings,
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
            // Dropped by interface edits; the first completion after one rebuilds.
            let ws = session.workspace();
            let reach =
                &*reach.get_or_insert_with(|| crate::reachability::ReachabilityIndex::build(ws));
            if let Some(items) = contextual_completions(session, reach, &rq, &cx) {
                return items;
            }
            let ws = session.workspace();
            let freq = &*freq.get_or_insert_with(|| build_freq_index(ws));
            let call_through = &*call_through.get_or_insert_with(|| build_call_through(ws, reach));
            let caches = Caches {
                reach,
                freq,
                call_through,
                scoped_call_through,
                visible_bindings,
            };
            general_completions(session, fallbacks, caches, &rq, &cx)
        });
        reconcile_suffix(&mut items, member_tail(src, offset));
        items
    }
}

/// The answers decided before classification: `Some` is the whole response.
fn lexical_gate(src: &str, offset: usize, space_triggered: bool) -> Option<Vec<CompletionItem>> {
    // Space-triggered on an all-whitespace line prefix == `<CR>` auto-indent,
    // not a real space-after-token context: suppress the popup.
    if space_triggered && line_prefix_is_blank(src, offset) {
        return Some(Vec::new());
    }
    // Only directives within comments and code inside interpolation holes complete.
    match rock_tokenizer::region_at(src, offset) {
        rock_tokenizer::LexRegion::Comment => {
            // Ordinary comments have no magic directive candidates.
            return Some(magic_comment_completions(src, offset).unwrap_or_default());
        }
        rock_tokenizer::LexRegion::StringText => return Some(Vec::new()),
        rock_tokenizer::LexRegion::Code => {}
    }
    // A token starting with a digit is a numeric literal, never an identifier
    // being typed (Rock idents are `[A-Za-z_][A-Za-z0-9_]*`) -- don't
    // subsequence-match it against symbols (`0` -> `remap_to_01`).
    if ident_prefix(src, offset)
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_digit)
    {
        return Some(Vec::new());
    }
    None
}

// A contextual lookup may decline: an incomplete receiver or qualifier can
// prevent the live file from lowering, and a block can parse as a record.
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
                        rq.src.get(brace_open + 1..rq.offset).unwrap_or("")
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
                    in_pattern_position(rq.src, rq.offset),
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
    // Try both repairs with inference before falling back to the named receiver.
    for named in [false, true] {
        if named && path.is_none() {
            break;
        }
        for text in rq.munges() {
            let candidates = analyze_variant(session, rq.path, text, |ws| {
                let mid = user_module_for_path(ws, rq.path)?;
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

/// Completion after a module path in a modules-v1 fragment
/// (`modules_scope::path_members`). `None` for a synthetic module or a prefix
/// that does not resolve to a module, leaving enum paths to their own source.
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
                        } else if ws.struct_by_name.contains_key(&key) {
                            CompletionItemKind::STRUCT
                        } else if ws.enums_by_name.contains_key(&key) {
                            CompletionItemKind::ENUM
                        } else if ws.newtypes_by_name.contains_key(&key) {
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

// -- General pipeline ---------------------------------------------------------

/// The resolved workspace view of the general pipeline.
struct GeneralCx<'w> {
    ws: &'w Workspace,
    mid: ModuleId,
    inf: &'w InferenceResult,
    /// The request file's fragment scope (modules v1).
    frag: &'w bone_core::modules::FragmentScope,
    /// The fn whose body holds the cursor. Inside its own body it must not
    /// be PROMOTED as a way to produce a value -- a call-through autofill, a
    /// reach path, or a fn-returning-the-slot-type -- because that is a
    /// recursive self-call, which buries the direct constructor the user is
    /// naming (a struct fill inside its own function body). It stays offered
    /// (plain candidate) for genuine recursion, just not lifted above
    /// type-directed candidates.
    enclosing_fn: Option<Ustr>,
    /// PatternPos: the cursor is in a match-arm pattern HEAD.
    pattern_pos: bool,
}

/// Scope and global namespace completion: gather every candidate, rank it,
/// add the synthesized items, then select and materialize the survivors.
fn general_completions(
    session: &mut Session,
    fallbacks: &FallbackReports,
    mut caches: Caches,
    rq: &Request,
    cx: &CompletionCx,
) -> Vec<CompletionItem> {
    // Use the scrutinee type to rank variants and suppress synthesized calls
    // and reach paths in a pattern.
    let pattern_pos = cx.pattern_scrutinee.is_some();
    let pattern_enum: Option<String> = cx
        .pattern_scrutinee
        .and_then(|a| pattern_scrutinee_enum(session, rq.path, rq.src, rq.offset, a));
    let session: &Session = session;
    let live = session.workspace();
    // Scope + global namespace needs a workspace with the user module.
    // Prefer the live session; but if the buffer is broken so the strict
    // session dropped the module, reparse it with RECOVERY so scope
    // completion still works mid-edit.
    let fallback_ws: Option<Workspace> = if user_module_for_path(live, rq.path).is_none() {
        recovery_workspace(session, fallbacks, rq, pattern_pos)
    } else {
        None
    };
    let ws: &Workspace = fallback_ws.as_ref().unwrap_or(live);
    let Some(mid) = user_module_for_path(ws, rq.path) else {
        return Vec::new();
    };
    let Some(frag) = crate::modules_scope::fragment(ws, ws.module(mid).file) else {
        return Vec::new();
    };
    let Some(inf) = ws.inference(mid) else {
        return Vec::new();
    };
    let enclosing_fn = ws
        .modules
        .get(mid.index())
        .and_then(|m| match m.body_at(rq.offset) {
            Some(bone_semantic::hir::BodyOwner::Function(fid)) => Some(m.functions[fid].name),
            _ => None,
        });
    let g = GeneralCx {
        ws,
        mid,
        inf,
        frag,
        enclosing_fn,
        pattern_pos,
    };

    // The prefix filter happens inside the completion query before
    // per-candidate detail rendering -- so typing narrows the work, not just
    // the result.
    let t_cands = bone_core::prelude::Instant::now();
    let mut cands = scope_candidates(
        &g,
        caches.visible_bindings,
        rq,
        cx.expr_position || pattern_pos,
    );
    // Macros are an expression, never a type.
    if cx.expr_position {
        push_macro_candidates(session, rq, &mut cands);
    }
    let import_edits =
        push_import_candidates(&g, session, rq, cx.expr_position || pattern_pos, &mut cands);
    let cands_ms = t_cands.elapsed().as_secs_f64() * 1000.0;
    let n_cands = cands.len();

    let mut expected = cx.expected(ws, mid, inf, rq.offset);
    let t_build = bone_core::prelude::Instant::now();
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
    // Partition by the same comparator that orders the final response.
    if general.len() > MAX_COMPLETION_ITEMS {
        general.select_nth_unstable_by(MAX_COMPLETION_ITEMS, cand_cmp);
        general.truncate(MAX_COMPLETION_ITEMS);
    }
    let mut sink = Sink::new();
    sink.cands = general;
    push_entity_synth(&g, rq, cx, &mut sink);
    push_named_args(&g, rq, cx, &mut sink);
    push_coroutine_synths(&g, rq, cx, &mut sink);
    let rank_ms = t_build.elapsed().as_secs_f64() * 1000.0;

    let t_reach = bone_core::prelude::Instant::now();
    // A pattern head is not a value slot: a dotted reach path is not a valid
    // pattern, so the expansion is suppressed there.
    if let Some(exp) = expected.as_ref() {
        if !exp.rendered.is_empty() && !pattern_pos {
            push_reach_paths(&g, caches.reach, rq, &exp.rendered, &mut sink);
        }
    }
    let reach_ms = t_reach.elapsed().as_secs_f64() * 1000.0;

    // Whole-call-through-first-param candidates (`rejtanog` ->
    // `reject_target(TargetError::NotGround)`). Expression position only (a
    // value slot), additive to the normal candidates. Suppressed in a
    // pattern head like reach: `f(arg)` is not a pattern.
    let t_ct = bone_core::prelude::Instant::now();
    let mut ct_counts = CtCounts::default();
    // When the cap is already met, skip call-through even though a local-seeded
    // call may outrank some entries. Narrowing the prefix can surface it.
    if cx.expr_position && !pattern_pos && sink.len() < MAX_COMPLETION_ITEMS {
        ct_counts = push_call_through(
            &g,
            &mut caches,
            rq,
            !has_expected,
            expected.as_mut(),
            &mut sink,
        );
    }
    let call_through_ms = t_ct.elapsed().as_secs_f64() * 1000.0;
    LAST_COMPLETION_PHASES.with(|c| {
        c.set(CompletionPhaseTimings {
            candidates_ms: cands_ms,
            rank_ms,
            reach_ms,
            call_through_ms,
            n_general: n_cands,
            n_out: sink.len(),
            ct_fns: ct_counts.fns,
            ct_arg_iters: ct_counts.arg_iters,
            ct_survivors: ct_counts.survivors,
        })
    });
    finish(&g, sink, rq, &import_edits)
}

/// Recovery workspace for a buffer whose live parse dropped the user module.
fn recovery_workspace(
    session: &Session,
    fallbacks: &FallbackReports,
    rq: &Request,
    pattern_pos: bool,
) -> Option<Workspace> {
    let (src, offset) = (rq.src, rq.offset.min(rq.src.len()));
    // A half-typed arm HEAD (`match s { co`) is what broke the parse:
    // complete it (` => {}` at the cursor) so the recovery reparse keeps the
    // match -- and its scrutinee, which the pattern rung resolves -- instead
    // of dropping the statement.
    let fixed = if pattern_pos {
        let mut s = String::with_capacity(src.len() + 8);
        s.push_str(&src[..offset]);
        s.push_str(" => {}");
        s.push_str(&src[offset..]);
        s
    } else {
        src.to_string()
    };
    reparse_with_project(
        session,
        fallbacks,
        rq.path,
        &balance_delimiters(fixed),
        "completion",
    )
}

/// The in-scope and visible-global candidates matching the prefix.
fn scope_candidates(
    g: &GeneralCx,
    visible_bindings: &mut HashMap<Ustr, Vec<crate::query::VisibleBinding>>,
    rq: &Request,
    expr_position: bool,
) -> Vec<crate::query::CompletionEntry> {
    let ws = g.ws;
    let file = ws.module(g.mid).file;
    let visible = visible_bindings
        .entry(file)
        .or_insert_with(|| {
            let mut global_def_files = HashMap::new();
            for module in &ws.modules {
                for key in module.global_by_name.keys() {
                    global_def_files.entry(*key).or_insert(module.file);
                }
            }
            let mut test_path_cache: FxHashMap<Ustr, bool> = FxHashMap::default();
            crate::modules_scope::bound_names(ws, g.frag, |_| true)
                .into_iter()
                .filter_map(|(name, key)| {
                    crate::query::resolve_visible_binding(
                        ws,
                        g.frag,
                        name,
                        key,
                        global_def_files.get(&key).copied(),
                        &mut test_path_cache,
                    )
                })
                .collect()
        })
        .as_slice();
    crate::query::completion_with_visible_bindings(
        ws,
        g.mid,
        g.inf,
        rq.offset,
        &rq.prefix,
        expr_position,
        visible,
    )
}

/// Macros are expanded away before bone lowering, so the workspace has no
/// record of them -- without this every macro in the tree is invisible to
/// completion. The Session's parse-time capture is the only source (the same
/// one ctags and semantic tokens read).
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
        // Overloaded macros (`log_once` has a 2- and a 3-param form) share
        // one name -- offer the first, like an overloaded fn.
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

/// Out-of-scope accessible definitions of a v1 fragment, each with the `use`
/// that brings it in, keyed by label. Only while a name is being typed: the
/// empty-prefix list is what the file already binds.
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
    /// `for x in |`: an iterable is the expected operand.
    for_in: bool,
    /// Inference's `for`-head judgment over the cursor body (Local candidate
    /// TyIds index its BodyInference); `Some` only in a ForIn slot.
    for_in_oracle: Option<ForInOracle<'w>>,
    /// After `yield`/`await`, coroutine-returning fns rank first (the
    /// `await frame(1)` delegation pattern).
    after_yield: bool,
    /// Test-scoped symbols demote outside test files (kind bucket 4).
    demote_tests: bool,
    expected: Option<&'a mut ExpectedArg<'w>>,
    has_expected: bool,
    freq: &'a HashMap<Ustr, u32>,
    /// The request's project root, for the proximity rung.
    root: String,
    /// Proximity tier per defining file, memoized per request.
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

    /// Proximity tier of a defining file vs the request (late prior: 0 same
    /// file, 1 same project root, 2 other workspace code, 3 stdlib/builtins).
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
        // Locality/kind ladder, always computed: variables first
        // (locals/params, then globals), then functions / structs / bare
        // enums / newtypes, then enum-member VARIANTS last. In a typed slot it
        // is the post-band tiebreak (a right-type local beats a right-type
        // global); with no expected type it is the primary kind bucket.
        // Structs sit in bucket 2 (NOT below functions): on an EMPTY prefix
        // the cap keeps the best `max_items`, and burying types under every
        // stdlib fn would truncate them away entirely; within bucket 2 the
        // `method` tiebreak already orders non-fns ahead of functions.
        let kind = match c.kind() {
            // Test-scoped symbols (tests/-dir or test_-named) sink to the
            // bottom kind bucket OUTSIDE test files; the band rung still
            // leads, so an explicitly typed `test_` prefix surfaces them.
            _ if self.demote_tests && c.test_scoped => 4,
            SymCat::Local | SymCat::Param => 0,
            // After `yield`/`await`, a coroutine fn is the expected operand
            // -- promote it above plain fns/types.
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
        // Smart-case leading rank, untyped slots only: measure against the
        // variant NAME (the user types it; `Enum::` uppercasing would skew
        // it). In a typed slot (and the pattern / for-in slots) the
        // expected-TYPE bucket must stay dominant over case, so leave case
        // neutral (0) there.
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
            // The user types the variant NAME (`nog` -> `NotGround`); score
            // against it, but keep the qualified score too so a `tarnog`
            // (Enum + variant) query still ranks.
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
        // A leading contiguous run (you typed the start of the name) beats a
        // scattered all-boundary match on a longer name -- the fine score
        // alone over-rewards boundary-rich names. Only reorders within a
        // band; well below fuzzy_score's whole-prefix +1000.
        let score = score + (lead as i32) * 100;
        // Coarse match band + exact-name flag: classified against the variant
        // tail too (the user types `Combat`, not `MobAiState::Combat`),
        // mirroring the score's vname handling.
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
                // Locals ride the recency rung instead.
                SymCat::Local | SymCat::Param => 0,
                _ => freq_bucket(self.freq.get(&freq_name).copied().unwrap_or(0)),
            },
            rec: c.rec(),
            len: RankKey::len_key(score, name.len()),
        };
        Cand::new(key, CandName::Sym(c.name), CandShape::Entry(c))
    }

    /// The typed ladder: 0 = a value (or variant) of the expected type, 1 = a
    /// fn returning it, 2 = else; all 0 when no type is inferable.
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

/// Frequency prior: workspace ref counts, log-bucketed.
fn freq_bucket(n: u32) -> u8 {
    if n >= FREQ_HOT {
        0
    } else if n >= FREQ_WARM {
        1
    } else {
        2
    }
}

/// Offer an Entity template in typed parameter declarations.
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

/// Offer unused parameter names. A spread may already supply some of them.
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

/// Coroutine ergonomics: `Wait` is the common host yield type (~75% of
/// `Coroutine<..>` annotations), so surface it. All gated on a `Wait` type
/// existing (the host's `Wait` is an enum); otherwise the generic
/// `Coroutine<$0>` snippet stands in.
fn push_coroutine_synths(g: &GeneralCx, rq: &Request, cx: &CompletionCx, sink: &mut Sink) {
    let has_wait = nominal_exists(g.ws, g.frag, "Wait");
    let q = rq.prefix.as_bytes();
    if cx.coroutine_generic {
        // (a) first generic arg of `Coroutine<..>` -> `Wait` ranks #0,
        // superseding the plain struct symbol.
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
        // (b) completing the `Coroutine` type name -> snippet forms replace
        // the bare type: `Coroutine<Wait>` first, `Coroutine<$0>` second.
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
    // (c) `co fn NAME()|` with no return type -> offer the `-> Coroutine<..>`
    // return type to the right of `)`.
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

/// Offer paths from visible roots to an expected type using cached DefKeys.
fn push_reach_paths(
    g: &GeneralCx,
    reach: &crate::reachability::ReachabilityIndex,
    rq: &Request,
    expected_types: &[String],
    sink: &mut Sink,
) {
    let (ws, frag) = (g.ws, g.frag);
    let pfx = rq.prefix.as_bytes();
    let show_private = rq.prefix.starts_with('_');
    let callable_spellings =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Callable);
    let type_spellings =
        crate::modules_scope::spellings_by_key(ws, frag, bone_core::modules::DefKind::Type);
    let body_owner = ws.module(g.mid).body_at(rq.offset);
    let body_inference = body_owner.and_then(|owner| g.inf.per_fn.get(&owner));
    let mut roots: Vec<(String, String, u8)> =
        crate::query::locals_in_scope(ws, g.mid, g.inf, rq.offset)
            .into_iter()
            .map(|(n, _k, ty, ty_id, _decl)| {
                let ty = ty_id.map_or(ty, |ty_id| {
                    crate::reachability::type_key_in_body(ws, body_inference, ty_id).to_string()
                });
                (n.to_string(), ty, 0)
            })
            .collect();
    roots.extend(
        crate::modules_scope::visible_globals(ws, frag)
            .into_iter()
            .filter(|global| show_private || !global.name.as_str().starts_with('_'))
            .map(|global| (global.name.to_string(), global.node_ty, 1)),
    );
    for (vname, vty, root_kind) in roots {
        let res = reach.paths_to(&vty, expected_types, REACH_CAP, Some((ws, frag)));
        for p in res.paths {
            // A path that CALLS the enclosing fn (`k.partition_style()`
            // inside `partition_style`) is a recursive self-reference -- skip
            // it, same reason the fn is not promoted as a candidate.
            if g.enclosing_fn
                .is_some_and(|f| reach_suffix_calls(&p.suffix, &f))
            {
                continue;
            }
            // Dedup / fuzzy-match / sort on the bare access form; the final
            // hop's parens/snippet are rendered from the flags.
            let Some(suffix) = crate::modules_scope::display_method_suffix_with_names(
                &p.suffix,
                &p.method_members,
                &callable_spellings,
            ) else {
                continue;
            };
            let final_ty =
                crate::modules_scope::display_type_with_names(ws, &p.final_ty, &type_spellings);
            let key = format!("{vname}.{suffix}");
            // Require a CLEAN match (word-boundary / run), not a
            // coincidental scatter, so `nog` doesn't drag in
            // `necro_cast.ima(g)e_dims()...` reach paths.
            if !pfx.is_empty() && !clean_match(key.as_bytes(), pfx) {
                continue;
            }
            if !sink.admit(&key) {
                continue;
            }
            // Rank stored fields with values and method results with calls.
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

fn push_call_through(
    g: &GeneralCx,
    caches: &mut Caches,
    rq: &Request,
    score_leads: bool,
    expected: Option<&mut ExpectedArg>,
    sink: &mut Sink,
) -> CtCounts {
    let ws = g.ws;
    let file = ws.module(g.mid).file;
    let scoped_index = caches
        .scoped_call_through
        .entry(file)
        .or_insert_with(|| build_scoped_call_through(ws, g.frag, caches.call_through));
    call_through_param_completions(
        ws,
        g.mid,
        g.inf,
        rq.offset,
        &rq.prefix,
        caches.call_through,
        g.frag,
        scoped_index,
        caches.reach,
        score_leads,
        expected,
        g.enclosing_fn.as_deref(),
        sink,
    )
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
