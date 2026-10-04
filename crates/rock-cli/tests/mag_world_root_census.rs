use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ARENA_ACCESSORS: &[&str] = &[
    "world_mobs",
    "world_mob_fixtures",
    "world_spawners",
    "world_doors",
    "world_corpses",
    "world_ground_items",
    "world_fuses",
    "world_walls",
];

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
struct Counts {
    world_state: usize,
    arena: usize,
    active_scene: usize,
    active_world: usize,
}

impl Counts {
    fn exceeds(self, bound: Self) -> bool {
        self.world_state > bound.world_state
            || self.arena > bound.arena
            || self.active_scene > bound.active_scene
            || self.active_world > bound.active_world
    }
}

#[derive(Clone)]
struct Function {
    name: String,
    file: String,
    body: String,
    min_arity: usize,
    max_arity: usize,
    receiver: bool,
    parameters: Vec<(String, String)>,
    fields: Arc<HashMap<(String, String), String>>,
}

struct CallSite {
    name: String,
    arguments: Option<Vec<String>>,
    offset: usize,
    arity: Option<usize>,
    receiver: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root")
        .to_path_buf()
}

fn rock_sources(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read MAG source directory") {
        let entry = entry.expect("read MAG source entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                rock_sources(&path, out);
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rock") {
            out.push(path);
        }
    }
}

// Preserve byte offsets while removing comments and strings, including the
// triple-quoted bodies used by inline(lua). This is deliberately a small
// source census lexer, not a second Rock parser.
fn code_view(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' && bytes.get(i + 1) == Some(&b'"')
            && bytes.get(i + 2) == Some(&b'"')
        {
            for slot in &mut out[i..i + 3] {
                *slot = b' ';
            }
            i += 3;
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes.get(i + 1) == Some(&b'"')
                    && bytes.get(i + 2) == Some(&b'"')
                {
                    for slot in &mut out[i..i + 3] {
                        *slot = b' ';
                    }
                    i += 3;
                    break;
                }
                if bytes[i] != b'\n' {
                    out[i] = b' ';
                }
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out[i] = b' ';
            i += 1;
            while i < bytes.len() {
                let escaped = bytes[i] == b'\\' && bytes.get(i + 1).is_some();
                if bytes[i] != b'\n' {
                    out[i] = b' ';
                }
                i += 1;
                if escaped {
                    if bytes[i - 1] != b'\n' {
                        out[i - 1] = b' ';
                    }
                    if i < bytes.len() {
                        if bytes[i] != b'\n' {
                            out[i] = b' ';
                        }
                        i += 1;
                    }
                } else if bytes[i - 1] == b'"' {
                    break;
                }
            }
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).expect("MAG source remains UTF-8 after census masking")
}

fn identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn identifier_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn arguments(source: &str, open: usize, type_arguments: bool) -> Option<Vec<&str>> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut parens = 1;
    let mut brackets = 0;
    let mut braces = 0;
    let mut angles = 0;
    let mut start = open + 1;
    let mut args = Vec::new();
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        match byte {
            b'(' => parens += 1,
            b')' => {
                parens -= 1;
                if parens == 0 {
                    let arg = source[start..index].trim();
                    if !arg.is_empty() {
                        args.push(arg);
                    }
                    return Some(args);
                }
            }
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'<' if type_arguments => angles += 1,
            b'>' if type_arguments && angles > 0 => angles -= 1,
            b',' if parens == 1 && brackets == 0 && braces == 0 && angles == 0 => {
                let arg = source[start..index].trim();
                if !arg.is_empty() {
                    args.push(arg);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

fn record_fields(source: &str) -> HashMap<(String, String), String> {
    let mut fields = HashMap::new();
    let mut record = None;
    for line in code_view(source).lines() {
        let line = line.trim().strip_prefix("pub ").unwrap_or(line.trim());
        if let Some(declaration) = line.strip_prefix("struct ")
            .or_else(|| line.strip_prefix("value "))
            .or_else(|| line.strip_prefix("nobox value "))
        {
            record = Some(declaration.split(['<', ' ', '{']).next().unwrap().to_string());
        } else if line == "}" {
            record = None;
        } else if let (Some(owner), Some((name, ty))) = (&record, line.split_once(':')) {
            if name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                let ty = ty.split('=').next().unwrap().trim().trim_end_matches(',').trim();
                fields.insert((owner.clone(), name.to_string()), ty.to_string());
            }
        }
    }
    fields
}

fn expression_type(expression: &str, bindings: &HashMap<String, String>,
    fields: &HashMap<(String, String), String>) -> Option<String> {
    let expression = expression.trim();
    if let Some(ty) = bindings.get(expression) { return Some(ty.clone()); }
    let mut ty = None;
    let mut start = 0;
    let mut depth = 0;
    let mut parts = Vec::new();
    for (index, ch) in expression.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '.' if depth == 0 => { parts.push(&expression[start..index]); start = index + 1; }
            _ => {}
        }
    }
    parts.push(&expression[start..]);
    for (index, part) in parts.iter().enumerate() {
        let (member, indexed) = part.split_once('[').map_or((*part, false), |(name, _)| (name, true));
        ty = if index == 0 {
            bindings.get(member.trim()).cloned()
        } else {
            let current = ty?;
            match member.trim() {
                "get()" if current.starts_with("Entity<") =>
                    Some(format!("{}?", current.strip_prefix("Entity<")?.strip_suffix('>')?)),
                "unwrap()" => Some(current.trim_end_matches('?').to_string()),
                field => {
                    let owner = current.split('<').next()?.trim_end_matches('?');
                    fields.get(&(owner.to_string(), field.to_string())).cloned()
                }
            }
        };
        if indexed {
            ty = Some(ty?.strip_prefix('[')?.strip_suffix(']')?.to_string());
        }
    }
    ty
}

fn call_argument_types(function: &Function, call: &CallSite) -> Vec<Option<String>> {
    let mut bindings: HashMap<_, _> = function.parameters.iter().cloned().collect();
    let view = code_view(&function.body);
    for line in view[..call.offset].lines() {
        let Some(binding) = line.trim().strip_prefix("let ") else { continue };
        let Some((declaration, value)) = binding.split_once('=') else { continue };
        let (name, ty) = if let Some((name, ty)) = declaration.split_once(':') {
            (name.trim(), Some(ty.trim().to_string()))
        } else {
            (declaration.trim(), expression_type(value, &bindings, &function.fields))
        };
        if name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            if let Some(ty) = ty { bindings.insert(name.to_string(), ty); }
            else { bindings.remove(name); }
        }
    }
    call.arguments.as_ref().map(|args| args.iter()
        .map(|arg| expression_type(arg, &bindings, &function.fields)).collect()).unwrap_or_default()
}

fn function_declarations(source: &str, path: &Path) -> Vec<Function> {
    let view = code_view(source);
    let mut result = Vec::new();
    let fields = Arc::new(record_fields(source));
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let declaration = trimmed
            .strip_prefix("fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "));
        if let Some(declaration) = declaration {
            let name = declaration
                .split_once('(')
                .map(|(name, _)| name.trim())
                .unwrap_or("");
            if !name.is_empty() && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.') {
                let signature_open = view[line_start..]
                    .find('(')
                    .map(|offset| line_start + offset)
                    .expect("Rock function has argument list");
                let signature_args = arguments(&view, signature_open, true);
                let parameters = signature_args.as_ref().map(|args| args.iter().map(|arg| {
                    let (name, ty) = arg.split_once(':').unwrap_or((arg, ""));
                    (name.trim().to_string(), ty.split('=').next().unwrap().trim().to_string())
                }).collect()).unwrap_or_default();
                let (min_arity, max_arity) = match signature_args {
                    Some(args) => {
                        let max = args.len();
                        // Defaults are uncommon here. An uncertain default is
                        // treated as optional so this census stays conservative.
                        let min = if args.iter().any(|arg| arg.contains('=')) { 0 } else { max };
                        (min, max)
                    }
                    None => (0, usize::MAX),
                };
                let open = view[line_start..]
                    .find('{')
                    .map(|offset| line_start + offset)
                    .unwrap_or_else(|| panic!("missing body for {name} in {}", path.display()));
                let mut depth = 0;
                let mut close = None;
                for (offset, byte) in view.as_bytes()[open..].iter().enumerate() {
                    match byte {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(open + offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let close = close.unwrap_or_else(|| panic!("unterminated {name} in {}", path.display()));
                result.push(Function {
                    name: name.to_string(),
                    file: path.strip_prefix(repo_root()).unwrap().display().to_string(),
                    body: source[open + 1..close].to_string(),
                    min_arity,
                    max_arity,
                    receiver: name.contains('.'),
                    parameters,
                    fields: fields.clone(),
                });
            }
        }
        line_start += line.len();
    }
    result
}

fn call_sites(source: &str) -> Vec<CallSite> {
    let view = code_view(source);
    let bytes = view.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !identifier_start(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && identifier_continue(bytes[i]) {
            i += 1;
        }
        let name = &view[start..i];
        let mut next = i;
        while next < bytes.len() && bytes[next].is_ascii_whitespace() {
            next += 1;
        }
        if next < bytes.len() && bytes[next] == b'('
            && !matches!(name, "if" | "for" | "while" | "match" | "fn" | "return"
                | "space_point" | "world_point")
        {
            let receiver = view[..start].trim_end().ends_with('.');
            result.push(CallSite {
                name: name.to_string(),
                arguments: arguments(&view, next, false).map(|args| args.into_iter().map(str::to_string).collect()),
                offset: start,
                arity: arguments(&view, next, false).map(|args| args.len()),
                receiver,
            });
        }
    }
    result
}

fn token_count(source: &str, token: &str) -> usize {
    let view = code_view(source);
    let bytes = view.as_bytes();
    let target = token.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + target.len() <= bytes.len() {
        if &bytes[i..i + target.len()] == target
            && (i == 0 || !identifier_continue(bytes[i - 1]))
            && (i + target.len() == bytes.len() || !identifier_continue(bytes[i + target.len()]))
        {
            count += 1;
            i += target.len();
        } else {
            i += 1;
        }
    }
    count
}

fn call_count(source: &str, name: &str) -> usize {
    call_sites(source).into_iter().filter(|call| call.name == name).count()
}

fn functions() -> Vec<Function> {
    let root = repo_root();
    let mut sources = Vec::new();
    rock_sources(&root.join("mag"), &mut sources);
    sources.sort();
    let mut functions = Vec::new();
    let mut fields = record_fields(&fs::read_to_string(root.join("mag-world/world_root.rock")).expect("read root types"));
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        fields.extend(record_fields(&text));
        functions.extend(function_declarations(&text, &source));
    }
    let fields = Arc::new(fields);
    for function in &mut functions { function.fields = fields.clone(); }
    functions
}

fn tick_reachable(functions: &[Function]) -> HashSet<usize> {
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, function) in functions.iter().enumerate() {
        let bare = function.name.rsplit('.').next().unwrap_or(&function.name);
        by_name.entry(function.name.as_str()).or_default().push(index);
        by_name.entry(bare).or_default().push(index);
    }
    let mut reachable = HashSet::new();
    let mut queue = VecDeque::new();
    for index in by_name.get("mag_simulation_tick").cloned().unwrap_or_default() {
        queue.push_back(index);
    }
    while let Some(index) = queue.pop_front() {
        if !reachable.insert(index) {
            continue;
        }
        for call in call_sites(&functions[index].body) {
            let argument_types = call_argument_types(&functions[index], &call);
            if let Some(targets) = by_name.get(call.name.as_str()) {
                for target in targets {
                    let candidate = &functions[*target];
                    let compatible = candidate.receiver == call.receiver
                        && call.arity.is_none_or(|arity| {
                            let supplied = arity + usize::from(call.receiver);
                            candidate.min_arity <= supplied && supplied <= candidate.max_arity
                        });
                    // Only the proven disjoint cache/space overloads are filtered; all other
                    // type relationships and unknown expressions stay conservative.
                    let types_match = call.receiver || argument_types.iter().zip(&candidate.parameters)
                        .all(|(actual, (_, expected))| actual.as_ref().is_none_or(|actual| {
                            !matches!((actual.as_str(), expected.as_str()),
                                ("FieldCache", "SpaceId") | ("SpaceId", "FieldCache"))
                        }));
                    if compatible && types_match && !reachable.contains(target) {
                        queue.push_back(*target);
                    }
                }
            }
        }
    }
    reachable
}

fn census_from(functions: &[Function]) -> (BTreeMap<String, Counts>, usize) {
    let reachable = tick_reachable(functions);
    let mut counts: BTreeMap<String, Counts> = BTreeMap::new();
    for index in reachable.iter().copied() {
        let function = &functions[index];
        let entry = counts.entry(function.file.clone()).or_default();
        entry.world_state += call_count(&function.body, "mag_world_state");
        entry.active_scene += call_count(&function.body, "active_world_scene");
        entry.active_world += token_count(&function.body, "MAG_ACTIVE_WORLD");
        for accessor in ARENA_ACCESSORS {
            entry.arena += call_count(&function.body, accessor);
        }
    }
    (counts, reachable.len())
}

fn census() -> (BTreeMap<String, Counts>, usize) {
    census_from(&functions())
}

#[test]
fn census_call_shape_keeps_real_access_and_rejects_import_name_collisions() {
    fn fixture(name: &str, body: &str, arity: usize, receiver: bool) -> Function {
        Function {
            name: name.to_string(),
            file: "mag/fixture.rock".to_string(),
            body: body.to_string(),
            min_arity: arity,
            max_arity: arity,
            receiver,
            parameters: Vec::new(),
            fields: Arc::new(HashMap::new()),
        }
    }
    let functions = vec![
        fixture("mag_simulation_tick", "tick_helper()", 0, false),
        fixture("tick_helper", "world_mobs(); recovery_run_start(state); recovery_run_complete(state)", 0, false),
        fixture("world_mobs", "", 0, false),
        fixture("recovery_run_start", "mag_world_state()", 2, false),
        fixture("Player.recovery_run_complete", "mag_world_state()", 2, true),
    ];
    let (counts, reachable) = census_from(&functions);
    assert_eq!(reachable, 3, "same-name wrappers with incompatible call shape stay off tick");
    assert_eq!(counts["mag/fixture.rock"].arena, 1, "the real accessor remains counted");
    assert_eq!(counts["mag/fixture.rock"].world_state, 0);

    let methods = vec![
        fixture("mag_simulation_tick", "player.complete(space)", 0, false),
        fixture("Player.complete", "mag_world_state()", 2, true),
    ];
    let (counts, reachable) = census_from(&methods);
    assert_eq!(reachable, 2, "receiver syntax supplies the declared receiver argument");
    assert_eq!(counts["mag/fixture.rock"].world_state, 1);
}

#[test]
fn census_resolves_field_cache_and_space_id_overloads_without_hiding_unknown_calls() {
    let source = r#"struct Layout {
    field: FieldCache,
}
struct OtherLayout {
    field: SpaceId,
}
struct Root {
    spaces: [Entity<Layout>],
}
struct World {
    root: Root,
}
fn mag_simulation_tick(field: FieldCache, world: World) {
    let layout = world.root.spaces[0].get().unwrap()
    field_query_near_dirty_terrain(v(0, 0), field)
    field_query_near_dirty_terrain(v(0, 0), layout.field)
}
fn field_query_near_dirty_terrain(ground: Vec2, field: FieldCache) { }
fn field_query_near_dirty_terrain(ground: Vec2, space: SpaceId) { mag_world_state() }
"#;
    let path = repo_root().join("mag/fixture.rock");
    let functions = function_declarations(source, &path);
    let (counts, reachable) = census_from(&functions);
    assert_eq!(reachable, 2, "typed cache calls must reach only the cache overload");
    assert_eq!(counts["mag/fixture.rock"].world_state, 0);

    for argument in ["space", "unknown"] {
        let source = source.replace("field: FieldCache, world: World", "space: SpaceId")
            .replace("field_query_near_dirty_terrain(v(0, 0), field)",
                &format!("field_query_near_dirty_terrain(v(0, 0), {argument})"))
            .replace("field_query_near_dirty_terrain(v(0, 0), layout.field)", "");
        let (counts, _) = census_from(&function_declarations(&source, &path));
        assert_eq!(counts["mag/fixture.rock"].world_state, 1,
            "SpaceId and unresolved calls must retain the ambient overload");
    }
}

#[test]
fn census_argument_scan_handles_nested_calls_and_optional_parameters() {
    let view = "f(inner(a, b), [x, y], named: { a: 1, b: 2 },)";
    assert_eq!(arguments(view, 1, false).unwrap().len(), 3);
    let sites = call_sites("actor.step(inner(a, b), [x, y])");
    assert_eq!(sites[0].name, "step");
    assert!(sites[0].receiver);
    assert_eq!(sites[0].arity, Some(2));
    let source = "fn helper(items: Map<int, [float]>, count: int = 1) { return count }";
    let parsed = function_declarations(source, &repo_root().join("mag/fixture.rock"));
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].min_arity, 0);
    assert_eq!(parsed[0].max_arity, 2);
}

fn baseline_bounds() -> BTreeMap<&'static str, Counts> {
    // Keep this empty once the census measures function bodies. Any new
    // tick-reachable implicit access must fail the gate until it is threaded
    // through the explicit world root.
    BTreeMap::new()
}

#[test]
fn tick_reachable_implicit_world_access_stays_within_shrink_only_bounds() {
    let (counts, reachable) = census();
    assert!(reachable > 0, "the census must find mag_simulation_tick");
    let bounds = baseline_bounds();
    let mut violations = Vec::new();
    for (file, actual) in &counts {
        let bound = bounds.get(file.as_str()).copied().unwrap_or_default();
        if actual.exceeds(bound) {
            violations.push(format!("{file}: actual={actual:?} bound={bound:?}"));
        }
    }
    assert!(violations.is_empty(),
        "tick-reachable implicit world access exceeds its shrink-only bounds (reachable functions: {reachable}):\n{}\nall counts:\n{}",
        violations.join("\n"),
        counts.iter().map(|(file, count)| format!("{file}: {count:?}")).collect::<Vec<_>>().join("\n"));
}
