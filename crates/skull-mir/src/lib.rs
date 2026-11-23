//! Cell-model CFG IR: variables are mutable cells, an `Idx<Inst>` is its single-use
//! value, control flow lives only in terminators, and merges go through cells
//! (no SSA, no block params).
pub mod append_vec;
pub mod bound;
pub mod cfg;
pub mod code_identity;
pub mod concrete;
pub mod decorator;
pub mod definition;
pub mod display;
pub mod global_storage;
pub mod gpu_layout;
pub mod instance;
pub mod intrinsics;
pub mod lifecycle;
pub mod lower;
pub mod lua_interop;
pub mod lua_nesting;
pub mod mangle;
pub mod mir;
pub mod monoty;
pub mod names;
pub mod nilness;
pub mod reach;
pub mod shader_ir;
pub mod sinking;
pub mod site;
mod striped;
pub mod struct_defaults;
pub mod type_table;
pub mod verdict;
pub mod verify;
pub mod vertex_plan;

const DISABLE_ENV: &str = "ROCK_MIR_DISABLE";
const SWITCHABLE: [&str; 13] = [
    "sroa",
    "copyprop",
    "foldcmp",
    "foldbr",
    "inline",
    "lambda",
    "forl",
    "bounds",
    "noboxflat",
    "mem2reg",
    "noboxfold",
    "latefloat",
    "callfwd",
];

/// Whether `ROCK_MIR_DISABLE` (a comma-separated list of pass names) turns
/// `pass` off. For differential testing of the optimizer: a pass that hides or
/// causes a miscompile shows up as a different result with it disabled. The
/// variable is read once; unset costs one load per call, and callers are
/// per-function or per-site, never per-instruction.
pub fn pass_disabled(pass: &str) -> bool {
    static OFF: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    debug_assert!(SWITCHABLE.contains(&pass), "unregistered pass {pass}");
    OFF.get_or_init(|| {
        let list = std::env::var(DISABLE_ENV).unwrap_or_default();
        let names: Vec<String> = list
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(bad) = names.iter().find(|n| !SWITCHABLE.contains(&n.as_str())) {
            panic!(
                "{DISABLE_ENV}: unknown pass `{bad}`; known: {}",
                SWITCHABLE.join(",")
            );
        }
        names
    })
    .iter()
    .any(|s| s == pass)
}

pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Written out rather than `DefaultHasher`: these hashes are emitted and must stay
/// byte-stable across toolchains.
pub(crate) fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(seed, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

/// An emitted numeric literal is exact in LuaJIT and JS only to 53 bits; the high
/// bits are XOR-folded down so they still contribute.
pub(crate) fn fold53(h: u64) -> u64 {
    (h ^ (h >> 53)) & ((1 << 53) - 1)
}
