//! The uniform-block entry points are declared TWICE: as `extern "C"` fns in
//! `src/uniform_block.rs`, and as a cdef + `extern(ffi, ..)` pair in
//! `stone/engine/lib.rock`. LuaJIT's ffi trusts the cdef, so a disagreement is
//! not a link error -- it reads arguments off the wrong stack slots.
//!
//! A drift is not a build failure, so without this guard it surfaces as garbage
//! bytes at a draw.

/// `(fn name, C parameter types in order)`, read off `src/uniform_block.rs`.
const EXPECTED: &[(&str, &[&str])] = &[
    (
        "stone_shader_bind_uniform_block",
        &[
            "StoneShaderHandle",
            "const char*",
            "const char*",
            "int",
            "int",
            "uint64_t",
        ],
    ),
    ("stone_unfilled_uniform_block_count", &[]),
    (
        "stone_shader_snapshot_uniform_block",
        &["StoneShaderHandle", "const char*", "uint8_t*", "int"],
    ),
    (
        "stone_shader_upload_uniform_block",
        &["StoneShaderHandle", "const uint8_t*", "int", "uint64_t"],
    ),
    (
        "stone_shader_check_uniform_block_layout",
        &[
            "StoneShaderHandle",
            "const char*",
            "const char*",
            "int",
            "const int*",
            "int",
            "int",
        ],
    ),
];

fn lib_rock() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("packages/stone/engine/lib.rock");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The cdef parameter types for `name`, as declared in the `setup_stone` cdef.
fn cdef_params(src: &str, name: &str) -> Vec<String> {
    let needle = format!(" {name}(");
    let at = src
        .find(&needle)
        .unwrap_or_else(|| panic!("lib.rock's cdef no longer declares `{name}`"));
    let open = at + needle.len();
    let close = src[open..].find(')').expect("cdef arg list ends") + open;
    let args = src[open..close].trim();
    if args.is_empty() {
        return Vec::new();
    }
    args.split(',')
        .map(|p| {
            // Drop the parameter NAME, keep the type -- `const char* block_name`
            // -> `const char*`, `int size` -> `int`.
            let p = p.trim();
            let cut = p.rfind([' ', '*']).expect("a typed param");
            p[..=cut].trim().to_string()
        })
        .collect()
}

#[test]
fn uniform_block_cdef_matches_the_rust_signatures() {
    let src = lib_rock();
    for (name, want) in EXPECTED {
        let got = cdef_params(&src, name);
        assert_eq!(
            got,
            want.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "cdef for `{name}` drifted from the Rust `extern \"C\"` signature"
        );
    }
}

/// The Rock `extern(ffi, ..)` declaration must take the same NUMBER of
/// arguments as the cdef. Rock spells the types its own way (`int`, `PtrU8`),
/// so arity is what is comparable -- and arity is what a forgotten parameter
/// changes.
#[test]
fn rock_externs_match_the_cdef_arity() {
    let src = lib_rock();
    for (name, want) in EXPECTED {
        let needle = format!("fn {name}(");
        let at = src
            .find(&needle)
            .unwrap_or_else(|| panic!("lib.rock no longer declares `extern fn {name}`"));
        let open = at + needle.len();
        let close = src[open..].find(')').expect("extern arg list ends") + open;
        let args = src[open..close].trim();
        let got = if args.is_empty() {
            0
        } else {
            args.split(',').count()
        };
        assert_eq!(
            got,
            want.len(),
            "`extern fn {name}` takes {got} args but its cdef takes {}",
            want.len()
        );
    }
}
