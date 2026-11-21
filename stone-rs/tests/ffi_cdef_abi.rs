//! Stone's Rust exports are declared again as `ffi.cdef` prototypes in the
//! Stone Rock packages. LuaJIT trusts the cdef, so a disagreement is not a
//! link error: it reads arguments off the wrong registers or stack slots.
//! This checks every cdef prototype against the Rust `extern "C"` signature,
//! every Rust export against some cdef, and every Rock `extern(ffi, "stone")`
//! against its cdef's arity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Exports reached without a cdef: Lua C modules the runtime opens and the
/// asset registry hosts fill from Rust.
const EXPORTS_WITHOUT_CDEF: &[&str] = &[
    "luaopen_stone_lua_alloc_counter",
    "luaopen_stone_lua_jit_health",
    "stone_register_asset",
];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ty {
    Void,
    Bool,
    Char,
    Int { bits: u8, signed: bool },
    Float(u8),
    Ptr(Box<Ty>),
    FnPtr,
    Named(String),
}

impl Ty {
    fn is_void_ptr(&self) -> bool {
        matches!(self, Ty::Ptr(inner) if **inner == Ty::Void)
    }
}

#[derive(Debug)]
struct Signature {
    params: Vec<Ty>,
    ret: Ty,
    origin: String,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn files_with_extension(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_with_extension(&path, extension, out);
        } else if path.extension().is_some_and(|e| e == extension) {
            out.push(path);
        }
    }
    out.sort();
}

fn read_sources(root: &Path, dir: &str, extension: &str) -> Vec<(String, String)> {
    let mut paths = Vec::new();
    files_with_extension(&root.join(dir), extension, &mut paths);
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let name = path.strip_prefix(root).unwrap().display().to_string();
            (name, text)
        })
        .collect()
}

fn strip_c_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |at| &after[at..]);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |at| &after[at + 2..]);
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// Every `ffi.cdef[[...]]` block body in `src`.
fn cdef_blocks(src: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find("cdef") {
        rest = &rest[at + 4..];
        let trimmed = rest.trim_start_matches([',', '(', ' ']);
        if let Some(body) = trimmed.strip_prefix("[[") {
            let end = body.find("]]").expect("cdef block closes");
            blocks.push(&body[..end]);
            rest = &body[end..];
        }
    }
    blocks
}

/// Top-level `;`-terminated declarations, ignoring `;` inside braces.
fn c_declarations(block: &str) -> Vec<String> {
    let mut decls = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in strip_c_comments(block).chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ';' if depth == 0 => {
                decls.push(current.split_whitespace().collect::<Vec<_>>().join(" "));
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    decls
}

/// A cdef prototype as spelled: parameter declarations, return type, file.
struct Prototype {
    params: Vec<String>,
    ret: String,
    file: String,
}

#[derive(Default)]
struct Cdef {
    typedefs: BTreeMap<String, String>,
    functions: BTreeMap<String, Vec<Prototype>>,
}

fn split_params(params: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in params.chars() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn cdef_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut sources = Vec::new();
    for rel in [
        "packages/stone/engine/stone.h",
        "packages/stone-native/stone_native.h",
        "packages/stone/engine/debug/lua_alloc_split.h",
    ] {
        sources.push((
            rel.to_string(),
            std::fs::read_to_string(root.join(rel)).unwrap(),
        ));
    }
    for (file, text) in read_sources(&root, "packages", "rock") {
        sources.extend(
            cdef_blocks(&text)
                .into_iter()
                .map(|body| (file.clone(), body.to_string())),
        );
    }
    sources
}

fn read_cdefs() -> Cdef {
    let mut cdef = Cdef::default();
    for (file, block) in cdef_sources() {
        for decl in c_declarations(&block) {
            if let Some(rest) = decl.strip_prefix("typedef ") {
                let name = rest
                    .rsplit([' ', '}', '*'])
                    .next()
                    .unwrap()
                    .trim()
                    .to_string();
                let target = if rest.starts_with("enum") {
                    "int".to_string()
                } else if rest.starts_with("struct") {
                    name.clone()
                } else {
                    rest[..rest.len() - name.len()].trim().to_string()
                };
                cdef.typedefs.insert(name, target);
                continue;
            }
            let Some(open) = decl.find('(') else { continue };
            if decl[..open].contains('{') || decl[open..].starts_with("(*") {
                continue;
            }
            let head = decl[..open].trim();
            let name_at = head.rfind([' ', '*']).map_or(0, |at| at + 1);
            let name = head[name_at..].to_string();
            let ret = head[..name_at].trim().to_string();
            let close = decl.rfind(')').expect("prototype closes");
            let params = decl[open + 1..close].trim();
            let params = if params == "void" {
                Vec::new()
            } else {
                split_params(params)
            };
            cdef.functions.entry(name).or_default().push(Prototype {
                params,
                ret,
                file: file.clone(),
            });
        }
    }
    cdef
}

/// A C parameter or return type, with any parameter name dropped.
fn c_type(spelling: &str, typedefs: &BTreeMap<String, String>, is_param: bool) -> Ty {
    if spelling.contains("(*") {
        return Ty::FnPtr;
    }
    let pointers = spelling.matches('*').count();
    let mut words: Vec<&str> = spelling
        .split(|c: char| c.is_whitespace() || c == '*')
        .filter(|w| !w.is_empty() && *w != "const" && *w != "struct")
        .collect();
    let is_base_word = |w: &str| {
        matches!(
            w,
            "unsigned" | "signed" | "long" | "int" | "short" | "char" | "float" | "double"
        )
    };
    if is_param && words.len() > 1 && !is_base_word(words.last().unwrap()) {
        words.pop();
    }
    let base = words.join(" ");
    let mut ty = match base.as_str() {
        "void" => Ty::Void,
        "bool" => Ty::Bool,
        "char" => Ty::Char,
        "int8_t" | "signed char" => Ty::Int {
            bits: 8,
            signed: true,
        },
        "uint8_t" | "unsigned char" => Ty::Int {
            bits: 8,
            signed: false,
        },
        "int16_t" | "short" => Ty::Int {
            bits: 16,
            signed: true,
        },
        "uint16_t" | "unsigned short" => Ty::Int {
            bits: 16,
            signed: false,
        },
        "int" | "int32_t" => Ty::Int {
            bits: 32,
            signed: true,
        },
        "unsigned" | "unsigned int" | "uint32_t" => Ty::Int {
            bits: 32,
            signed: false,
        },
        "long long" | "int64_t" | "intptr_t" | "ptrdiff_t" => Ty::Int {
            bits: 64,
            signed: true,
        },
        "unsigned long long" | "uint64_t" | "uintptr_t" | "size_t" => Ty::Int {
            bits: 64,
            signed: false,
        },
        "float" => Ty::Float(32),
        "double" => Ty::Float(64),
        other => match typedefs
            .get(other)
            .filter(|target| *target != other)
            .map(|target| c_type(target, typedefs, false))
        {
            Some(Ty::Named(_)) | None => Ty::Named(other.to_string()),
            Some(resolved) => resolved,
        },
    };
    for _ in 0..pointers {
        ty = Ty::Ptr(Box::new(ty));
    }
    ty
}

fn rust_type(spelling: &str, aliases: &BTreeMap<String, String>) -> Ty {
    let s = spelling.trim();
    if s.is_empty() || s == "()" {
        return Ty::Void;
    }
    if let Some(inner) = s
        .strip_prefix("*const ")
        .or_else(|| s.strip_prefix("*mut "))
    {
        return Ty::Ptr(Box::new(rust_type(inner, aliases)));
    }
    if s.starts_with("Option<") || s.contains("fn(") {
        return Ty::FnPtr;
    }
    let last = s.rsplit("::").next().unwrap();
    match last {
        "c_void" => Ty::Void,
        "bool" => Ty::Bool,
        "c_char" => Ty::Char,
        "i8" | "c_schar" => Ty::Int {
            bits: 8,
            signed: true,
        },
        "u8" | "c_uchar" => Ty::Int {
            bits: 8,
            signed: false,
        },
        "i16" | "c_short" => Ty::Int {
            bits: 16,
            signed: true,
        },
        "u16" | "c_ushort" => Ty::Int {
            bits: 16,
            signed: false,
        },
        "i32" | "c_int" => Ty::Int {
            bits: 32,
            signed: true,
        },
        "u32" | "c_uint" => Ty::Int {
            bits: 32,
            signed: false,
        },
        "i64" | "c_longlong" | "isize" => Ty::Int {
            bits: 64,
            signed: true,
        },
        "u64" | "c_ulonglong" | "usize" => Ty::Int {
            bits: 64,
            signed: false,
        },
        "f32" | "c_float" => Ty::Float(32),
        "f64" | "c_double" => Ty::Float(64),
        other => match aliases.get(other) {
            Some(target) => rust_type(target, aliases),
            None => Ty::Named(other.to_string()),
        },
    }
}

/// `type X = Y;` aliases, and `#[repr(C)]` enums as the `int` C passes them as.
fn rust_aliases(sources: &[(String, String)]) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    for (_, text) in sources {
        for line in text.lines() {
            let line = line
                .trim()
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ");
            if let Some((name, target)) = line
                .strip_prefix("type ")
                .and_then(|rest| rest.strip_suffix(';'))
                .and_then(|rest| rest.split_once(" = "))
            {
                aliases.insert(name.trim().to_string(), target.trim().to_string());
            }
        }
        let mut rest = text.as_str();
        while let Some(at) = rest.find("#[repr(C)]") {
            rest = &rest[at..];
            let header = &rest[..rest.find('{').unwrap_or(rest.len())];
            if let Some(name) = header.split("enum ").nth(1) {
                aliases.insert(name.trim().to_string(), "i32".to_string());
            }
            rest = &rest[1..];
        }
    }
    aliases
}

/// The text from `open` (an opening paren) through its matching close.
fn balanced(src: &str, open: usize) -> &str {
    let mut depth = 0;
    for (i, ch) in src[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parameter list");
}

fn rust_exports() -> BTreeMap<String, Vec<Signature>> {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = read_sources(&src_dir, "", "rs");
    let aliases = rust_aliases(&sources);
    let mut exports: BTreeMap<String, Vec<Signature>> = BTreeMap::new();
    for (file, text) in &sources {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("#[no_mangle]") {
            rest = &rest[at + "#[no_mangle]".len()..];
            let item_end = rest.find(['{', ';']).unwrap_or(rest.len());
            let Some(fn_at) = rest[..item_end].find("extern \"C\" fn ") else {
                continue;
            };
            let name_start = fn_at + "extern \"C\" fn ".len();
            let open = name_start + rest[name_start..].find('(').unwrap();
            let name = rest[name_start..open].trim().to_string();
            let params = balanced(rest, open);
            let after = &rest[open + params.len() + 2..];
            let body = after.find('{').unwrap();
            let ret = after[..body].trim().strip_prefix("->").unwrap_or("").trim();
            let params = split_params(params)
                .iter()
                .map(|p| rust_type(p.split_once(':').expect("typed parameter").1, &aliases))
                .collect();
            exports.entry(name).or_default().push(Signature {
                params,
                ret: rust_type(ret, &aliases),
                origin: file.clone(),
            });
        }
    }
    exports
}

fn compatible(rust: &Ty, c: &Ty, is_return: bool) -> bool {
    if rust == c || rust.is_void_ptr() && matches!(c, Ty::Ptr(_)) {
        return true;
    }
    match (rust, c) {
        (Ty::Ptr(_), Ty::Ptr(_)) => c.is_void_ptr(),
        // A Rust bool must hold 0 or 1: reading one as a byte is fine, the reverse is not.
        (Ty::Bool, Ty::Int { bits: 8, .. }) => is_return,
        (Ty::Int { bits: 8, .. }, Ty::Bool) => true,
        (Ty::Named(r), Ty::Named(c)) => r == c || format!("Stone{r}") == *c,
        _ => false,
    }
}

#[test]
fn cdef_and_rust_exports_agree() {
    let cdef = read_cdefs();
    let exports = rust_exports();
    let mut problems: Vec<_> = exports
        .keys()
        .filter(|name| {
            !cdef.functions.contains_key(*name) && !EXPORTS_WITHOUT_CDEF.contains(&name.as_str())
        })
        .map(|name| format!("{name}: exported without a cdef"))
        .collect();
    for (name, decls) in &cdef.functions {
        if !name.starts_with("stone_") {
            continue;
        }
        let Some(rust) = exports.get(name) else {
            problems.push(format!(
                "{name}: declared in {} but not exported",
                decls[0].file
            ));
            continue;
        };
        for Prototype { params, ret, file } in decls {
            let params: Vec<Ty> = params
                .iter()
                .map(|p| c_type(p, &cdef.typedefs, true))
                .collect();
            let ret = c_type(ret, &cdef.typedefs, false);
            for sig in rust {
                let mismatch = params.len() != sig.params.len()
                    || !compatible(&sig.ret, &ret, true)
                    || sig
                        .params
                        .iter()
                        .zip(&params)
                        .any(|(r, c)| !compatible(r, c, false));
                if mismatch {
                    problems.push(format!(
                        "{name}: cdef in {file} is ({params:?}) -> {ret:?}, \
                         Rust in {} is ({:?}) -> {:?}",
                        sig.origin, sig.params, sig.ret
                    ));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Rock spells parameter types its own way, so arity and whether a value
/// comes back are what is comparable with the cdef.
#[test]
fn rock_externs_match_their_cdef_shape() {
    let cdef = read_cdefs();
    let mut problems = Vec::new();
    for (file, text) in read_sources(&workspace_root(), "packages", "rock") {
        let needle = "extern(ffi, \"stone\") fn ";
        let mut rest = text.as_str();
        while let Some(at) = rest.find(needle) {
            rest = &rest[at + needle.len()..];
            let open = rest.find('(').unwrap();
            let name = &rest[..open];
            let params = balanced(rest, open);
            let after = &rest[open + params.len() + 2..];
            let returns = after
                .lines()
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with("->");
            let Some(decls) = cdef.functions.get(name) else {
                problems.push(format!("{file}: `{name}` has no cdef"));
                continue;
            };
            let Prototype {
                params: c_params,
                ret: c_ret,
                ..
            } = &decls[0];
            let arity = split_params(params).len();
            // A struct-valued extern returns through a leading out-pointer.
            let out_param = returns && c_ret == "void" && c_params.len() == arity + 1;
            if !out_param && (arity != c_params.len() || returns != (c_ret != "void")) {
                problems.push(format!(
                    "{file}: `{name}` takes {arity} args (returns: {returns}), \
                     its cdef takes {} and returns `{c_ret}`",
                    c_params.len()
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn stone_event_cdef_layout_matches_rust() {
    let header =
        std::fs::read_to_string(workspace_root().join("packages/stone/engine/stone.h")).unwrap();
    let decls: Vec<String> = c_declarations(&header)
        .into_iter()
        .filter(|decl| {
            decl.starts_with("typedef")
                && ["StoneEventType", "StoneKeyCode", "StoneEvent"]
                    .iter()
                    .any(|name| decl.ends_with(&format!(" {name}")))
        })
        .collect();
    assert_eq!(decls.len(), 3, "StoneEvent cdef types not found");
    use std::mem::{offset_of, size_of, size_of_val};
    use stone_rs::StoneEvent as E;

    let fields = [
        ("type", offset_of!(E, event_type)),
        ("key", offset_of!(E, key)),
        ("scancode", offset_of!(E, scancode)),
        ("modifiers", offset_of!(E, modifiers)),
        ("repeat", offset_of!(E, repeat)),
        ("mouse_button", offset_of!(E, mouse_button)),
        ("mouse_x", offset_of!(E, mouse_x)),
        ("mouse_y", offset_of!(E, mouse_y)),
        ("pad_instance_id", offset_of!(E, pad_instance_id)),
        ("pad_button", offset_of!(E, pad_button)),
        ("pad_axis", offset_of!(E, pad_axis)),
        ("pad_value", offset_of!(E, pad_value)),
        ("pad_sync", offset_of!(E, pad_sync)),
        ("text", offset_of!(E, text)),
    ];
    let script = format!(
        "local ffi = require('ffi')\nffi.cdef[[{};]]\n\
         local out = {{ ffi.sizeof('StoneEvent'), ffi.sizeof(ffi.new('StoneEvent').text), \
         tonumber(ffi.C.STONE_EVENT_TEXTINPUT) }}\n\
         for _, f in ipairs({{ '{}' }}) do out[#out + 1] = ffi.offsetof('StoneEvent', f) end\n\
         print(table.concat(out, ' '))",
        decls.join(";\n"),
        fields.map(|(name, _)| name).join("', '")
    );
    let output = std::process::Command::new(gamelibs::host_dir().join("bin/luajit"))
        .args(["-e", &script])
        .output()
        .expect("run the bundled LuaJIT");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luajit: Vec<usize> = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    let rust = [
        size_of::<E>(),
        size_of_val(&E::default().text),
        stone_rs::StoneEventType::TextInput as usize,
    ]
    .into_iter()
    .chain(fields.map(|(_, offset)| offset))
    .collect::<Vec<_>>();
    assert_eq!(luajit, rust);
}

/// On macOS arm64 the ninth and later integer-class arguments go on the stack,
/// packed to their natural size. LuaJIT's JIT sizes those stack slots from the
/// IR type including its guard bit, so a guarded 32-bit value (a checked
/// parameter or table load) is stored at a garbage offset and the callee reads
/// whatever was there. A 64-bit argument is converted by an unguarded `CONV`
/// first, and a Lua boolean is recorded as a constant, so both are safe.
#[test]
fn no_narrow_integer_arguments_on_the_stack() {
    const ARGUMENT_REGISTERS: usize = 8;
    let cdef = read_cdefs();
    let mut problems = Vec::new();
    for (name, decls) in &cdef.functions {
        for Prototype { params, file, .. } in decls {
            let mut gpr = 0;
            for spelling in params {
                if spelling == "..." {
                    break;
                }
                let ty = c_type(spelling, &cdef.typedefs, true);
                if matches!(ty, Ty::Float(_)) {
                    continue;
                }
                if gpr >= ARGUMENT_REGISTERS && matches!(ty, Ty::Int { bits: ..64, .. } | Ty::Char)
                {
                    problems.push(format!("{name} in {file}: stack argument `{spelling}`"));
                }
                gpr += 1;
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
