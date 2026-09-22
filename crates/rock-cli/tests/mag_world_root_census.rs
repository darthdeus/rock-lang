use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

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
    surface: String,
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

fn function_declarations(source: &str, path: &Path) -> Vec<Function> {
    let view = code_view(source);
    let mut result = Vec::new();
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
                    surface: source[line_start..close].to_string(),
                });
            }
        }
        line_start += line.len();
    }
    result
}

fn call_names(source: &str) -> Vec<String> {
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
            result.push(name.to_string());
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
    call_names(source).into_iter().filter(|call| call == name).count()
}

fn functions() -> Vec<Function> {
    let root = repo_root();
    let mut sources = Vec::new();
    rock_sources(&root.join("mag"), &mut sources);
    sources.sort();
    let mut functions = Vec::new();
    for source in sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        functions.extend(function_declarations(&text, &source));
    }
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
        for call in call_names(&functions[index].body) {
            if let Some(targets) = by_name.get(call.as_str()) {
                for target in targets {
                    if !reachable.contains(target) {
                        queue.push_back(*target);
                    }
                }
            }
        }
    }
    reachable
}

fn census() -> (BTreeMap<String, Counts>, usize) {
    let functions = functions();
    let reachable = tick_reachable(&functions);
    let mut counts: BTreeMap<String, Counts> = BTreeMap::new();
    for index in reachable.iter().copied() {
        let function = &functions[index];
        let entry = counts.entry(function.file.clone()).or_default();
        entry.world_state += call_count(&function.surface, "mag_world_state");
        entry.active_scene += call_count(&function.surface, "active_world_scene");
        entry.active_world += token_count(&function.surface, "MAG_ACTIVE_WORLD");
        for accessor in ARENA_ACCESSORS {
            entry.arena += call_count(&function.surface, accessor);
        }
    }
    (counts, reachable.len())
}

fn baseline_bounds() -> BTreeMap<&'static str, Counts> {
    // Filled from the RED census run. These are upper bounds, never equality
    // expectations, so a production migration can only make them smaller.
    // The world-state allowances below are the intentional floor-object
    // sentinel: their paths cross the protected ground-item/corpse slice.
    BTreeMap::from([
        ("mag/aggressors.rock", Counts { arena: 2, ..Counts::default() }),
        ("mag/blocker_index.rock", Counts { world_state: 3, arena: 1, ..Counts::default() }),
        ("mag/combat.rock", Counts { world_state: 1, arena: 1, ..Counts::default() }),
        ("mag/combat_text.rock", Counts { world_state: 3, ..Counts::default() }),
        ("mag/corpses.rock", Counts { world_state: 1, arena: 3, ..Counts::default() }),
        ("mag/doors.rock", Counts { world_state: 1, arena: 3, ..Counts::default() }),
        ("mag/editor_layout.rock", Counts { world_state: 6, ..Counts::default() }),
        ("mag/field.rock", Counts { world_state: 13, arena: 2, ..Counts::default() }),
        ("mag/flow.rock", Counts { world_state: 13, ..Counts::default() }),
        ("mag/fuses.rock", Counts { arena: 4, ..Counts::default() }),
        ("mag/ground_items.rock", Counts { arena: 2, ..Counts::default() }),
        ("mag/harvest.rock", Counts { world_state: 3, ..Counts::default() }),
        ("mag/hazards.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/lighting_semantics.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/los.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/magic.rock", Counts { arena: 1, ..Counts::default() }),
        ("mag/magic_trials.rock", Counts { arena: 2, ..Counts::default() }),
        ("mag/main.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/mob_spatial.rock", Counts { world_state: 8, ..Counts::default() }),
        ("mag/mobs.rock", Counts { world_state: 2, arena: 2, ..Counts::default() }),
        ("mag/notoriety.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/npc.rock", Counts { arena: 2, ..Counts::default() }),
        ("mag/outcomes.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/player.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/procgen/dungeon_runner.rock", Counts { arena: 1, ..Counts::default() }),
        ("mag/props.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/quest_catalog.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/quests.rock", Counts { world_state: 2, arena: 1, ..Counts::default() }),
        ("mag/restoration.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/searchable_props.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/sfx.rock", Counts { world_state: 2, ..Counts::default() }),
        ("mag/spawners.rock", Counts { arena: 4, ..Counts::default() }),
        ("mag/spells.rock", Counts { arena: 1, ..Counts::default() }),
        ("mag/summons.rock", Counts { arena: 3, ..Counts::default() }),
        ("mag/targetting.rock", Counts { arena: 1, ..Counts::default() }),
        ("mag/town_guards.rock", Counts { world_state: 1, arena: 2, ..Counts::default() }),
        ("mag/tutorial.rock", Counts { world_state: 1, arena: 1, ..Counts::default() }),
        ("mag/walls.rock", Counts { world_state: 1, arena: 8, ..Counts::default() }),
        ("mag/wisp_intent.rock", Counts { arena: 2, ..Counts::default() }),
        ("mag/world_address.rock", Counts { world_state: 1, ..Counts::default() }),
        ("mag/world_levels.rock", Counts { world_state: 23, arena: 7, active_scene: 2, ..Counts::default() }),
    ])
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
