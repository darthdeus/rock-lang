use lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, SemanticTokensServerCapabilities,
};

/// Translate bone's token-type AND token-modifier indices (into bone's own
/// legends, `bone_query::semantic_tokens_legend` / `_modifiers_legend`) to
/// indices into the legend THIS server advertises at initialize
/// (`get_semantic_tokens_legend`). bone classifies into its own legend; without
/// this remap the client decodes each token against the wrong legend (bone's
/// `0`=KEYWORD reads as the advertised `0`=NAMESPACE; bone's modifier bit
/// `0`=nullable reads as the advertised bit `0`=declaration), miscoloring the
/// whole buffer. Every bone type/modifier exists in the advertised legend, so
/// each lookup hits; the `unwrap_or(0)` is a defensive floor.
fn remap_bone_token_types(tokens: &mut lsp_types::SemanticTokens) {
    let advertised = get_semantic_tokens_legend();
    let bone_legend = bone_query::semantic_tokens_legend();
    let type_map: Vec<u32> = bone_legend
        .iter()
        .map(|t| advertised.token_types.iter().position(|a| a == t).unwrap_or(0) as u32)
        .collect();
    let bone_mods = bone_query::semantic_tokens_modifiers_legend();
    let mod_map: Vec<u32> = bone_mods
        .iter()
        .map(|m| {
            advertised
                .token_modifiers
                .iter()
                .position(|a| a == m)
                .unwrap_or(0) as u32
        })
        .collect();
    for tok in &mut tokens.data {
        if let Some(&idx) = type_map.get(tok.token_type as usize) {
            tok.token_type = idx;
        }
        if tok.token_modifiers_bitset != 0 {
            let mut remapped = 0u32;
            for (i, &target) in mod_map.iter().enumerate() {
                if tok.token_modifiers_bitset & (1 << i) != 0 {
                    remapped |= 1 << target;
                }
            }
            tok.token_modifiers_bitset = remapped;
        }
    }
}

/// Get the semantic tokens legend for Rock
pub fn get_semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::MACRO,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::REGEXP,
            SemanticTokenType::OPERATOR,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DEPRECATED,
            SemanticTokenModifier::ABSTRACT,
            SemanticTokenModifier::ASYNC,
            SemanticTokenModifier::MODIFICATION,
            SemanticTokenModifier::DOCUMENTATION,
            SemanticTokenModifier::DEFAULT_LIBRARY,
            // Custom Rock modifier — applied to type tokens where the AST
            // TypeExpr has is_nullable=true (i.e. `Foo?`). Themes can style
            // nullable types distinctly via `@lsp.mod.nullable.rock` or
            // `@lsp.typemod.struct.nullable.rock` etc.
            SemanticTokenModifier::new("nullable"),
            // Custom Rock modifier — applied to a variable / parameter /
            // global / struct-field READ when the binding's declared
            // type is `T?` but flow-sensitive analysis at this position
            // has narrowed it to `T` (e.g. inside `if x != null { ... }`,
            // after `?? default`, in match-arm null guards). Mutually
            // exclusive with `nullable` on the same token — the server
            // emits exactly one of the two for any read of a nullable
            // binding. Declarations always carry `nullable` because no
            // flow narrowing applies to a binding site.
            SemanticTokenModifier::new("refined"),
        ],
    }
}

/// Get semantic tokens server capabilities
pub fn get_semantic_tokens_capabilities() -> SemanticTokensServerCapabilities {
    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
        work_done_progress_options: Default::default(),
        legend: get_semantic_tokens_legend(),
        // nvim requests range for the initial paint / scroll (confirmed by
        // driving the real client); the handler classifies only viewport bodies.
        range: Some(true),
        // Advertise delta: on a re-request the client sends the previous
        // result_id and we reply with just the changed token span instead of the
        // whole array -- on a 12k-line file that's ~20ms -> ~the compute floor.
        full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
    })
}

/// Minimal `SemanticTokensEdit` list turning `old` into `new`: the common
/// prefix/suffix is unchanged, so one splice replaces the differing middle.
/// `start`/`delete_count` are in FLAT integer units (5 per token), per the LSP
/// spec (edits operate on the previous response's `data` array). Empty when the
/// arrays are identical.
pub fn semantic_tokens_delta_edits(
    old: &[SemanticToken],
    new: &[SemanticToken],
) -> Vec<lsp_types::SemanticTokensEdit> {
    let max_pre = old.len().min(new.len());
    let mut p = 0;
    while p < max_pre && old[p] == new[p] {
        p += 1;
    }
    let max_suf = (old.len() - p).min(new.len() - p);
    let mut s = 0;
    while s < max_suf && old[old.len() - 1 - s] == new[new.len() - 1 - s] {
        s += 1;
    }
    let removed = old.len() - p - s;
    let added = &new[p..new.len() - s];
    if removed == 0 && added.is_empty() {
        return Vec::new();
    }
    vec![lsp_types::SemanticTokensEdit {
        start: (p * 5) as u32,
        delete_count: (removed * 5) as u32,
        data: Some(added.to_vec()),
    }]
}

/// Range semantic tokens: bone's viewport classifier, remapped to the
/// advertised legend.
pub fn compute_semantic_tokens_range_workspace(
    workspace: &mut crate::LspWorkspace,
    params: SemanticTokensRangeParams,
) -> anyhow::Result<Option<SemanticTokensRangeResult>> {
    let uri_str = params.text_document.uri.as_str();
    let file_path = uri_str.strip_prefix("file://").unwrap_or(uri_str);
    let mut tokens = workspace.bone.semantic_tokens_range(file_path, params.range);
    remap_bone_token_types(&mut tokens);
    Ok(Some(SemanticTokensRangeResult::Tokens(tokens)))
}

/// Full-document semantic tokens: bone's tokenizer-based classifier (same one
/// the `--backend bone` CLI uses), remapped to the advertised legend.
/// `file_path` matches the key `bone.update` registers on didChange.
pub fn compute_semantic_tokens_workspace(
    workspace: &mut crate::LspWorkspace,
    params: SemanticTokensParams,
) -> anyhow::Result<Option<SemanticTokensResult>> {
    let uri_str = params.text_document.uri.as_str();
    let file_path = if uri_str.starts_with("file://") {
        &uri_str[7..]
    } else {
        uri_str
    };
    let mut tokens = workspace.bone.semantic_tokens(file_path);
    remap_bone_token_types(&mut tokens);
    Ok(Some(SemanticTokensResult::Tokens(tokens)))
}
