//! A TOY, math-only JS intrinsic table (docs/intrinsics-plan.md, slice 4): the
//! THIRD [`IntrinsicTable`] shape after luajit (in-source `inline(lua)` bodies)
//! and GLSL. Its only job is to keep the registry trait honest against more than
//! one real consumer -- an abstraction designed against exactly one consumer
//! fits exactly one consumer.
//!
//! NOT a backend: no runtime, no node, no program emission -- just the mapping
//! table + the call-site render primitive, exercised by snippet unit tests. The
//! seed of a future JS backend.

use bone_semantic::hir::Function;
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};

/// (name, arity) -> `$1..$n` JS expression template. Math-only (the ten in
/// docs/intrinsics-plan.md); a real JS backend grows this. Single source for
/// both [`JsIntrinsics::lookup`] and [`JsIntrinsics::entries`].
pub const JS_INTRINSICS: &[(&str, usize, &str)] = &[
    ("sin", 1, "Math.sin($1)"),
    ("cos", 1, "Math.cos($1)"),
    ("sqrt", 1, "Math.sqrt($1)"),
    ("abs", 1, "Math.abs($1)"),
    ("floor", 1, "Math.floor($1)"),
    ("ceil", 1, "Math.ceil($1)"),
    ("pow", 2, "Math.pow($1, $2)"),
    ("min", 2, "Math.min($1, $2)"),
    ("max", 2, "Math.max($1, $2)"),
    ("atan2", 2, "Math.atan2($1, $2)"),
];

/// The toy JS backend's [`IntrinsicTable`].
pub struct JsIntrinsics;

impl IntrinsicTable for JsIntrinsics {
    fn backend(&self) -> &'static str {
        "js"
    }
    fn lookup(&self, f: &Function) -> Option<Emission> {
        let n = f.name.as_str();
        let arity = f.param_decls.len();
        JS_INTRINSICS
            .iter()
            .find(|(name, ar, _)| *name == n && *ar == arity)
            .map(|(_, _, tmpl)| Emission::Template(tmpl))
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        JS_INTRINSICS
            .iter()
            .map(|(name, arity, _)| IntrinsicEntry { name, arity: *arity })
            .collect()
    }
}

/// Fill `$1..$n` in an expression template with `args` -- the call-site render
/// primitive a real JS codegen would use (the same shape GLSL uses). Kept here
/// so the `Emission::Template` contract is exercised end to end by a test.
pub fn fill_template(template: &str, args: &[&str]) -> String {
    let mut s = template.to_string();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("${}", i + 1), a);
    }
    s
}
