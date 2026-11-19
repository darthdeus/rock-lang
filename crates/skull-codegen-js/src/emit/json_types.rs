//! `__rock_json_types`: the typed-JSON record walker's view of the shared type
//! table (`skull_mir::type_table`), rendered for JS and limited to the types a
//! record conversion in this program can reach.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bone_core::workspace::Workspace;
use skull_mir::lower::{RepArena, ValueStorage};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::MonoInterner;
use skull_mir::type_table::{Entry, FieldEntry, Shape, VariantEntry};
use skull_parser::CompilerError;
use ustr::Ustr;

use super::js_string_lit;

/// The name an `inline(js)` body uses to read the table. A body that names it
/// with a type argument roots that type's closure.
pub(super) const TABLE: &str = "__rock_json_types";

pub(super) struct JsonTypes {
    arena: Arc<RepArena>,
    interner: Arc<MonoInterner>,
    used: bool,
    entries: BTreeMap<Ustr, Vec<Entry>>,
}

impl JsonTypes {
    pub(super) fn new(scx: &skull_mir::lower::ScalarCtx) -> Self {
        Self {
            arena: Arc::clone(scx.rep_arena()),
            interner: scx.interner_handle(),
            used: false,
            entries: BTreeMap::new(),
        }
    }

    /// Record an `inline(js)` body. Returns the field factories the newly
    /// reached entries call, which must be emitted with the program.
    pub(super) fn add_body(
        &mut self,
        ws: &Workspace,
        decl: FuncRef,
        args: &[skull_mir::monoty::MonoTyId],
    ) -> Result<Vec<FuncRef>, CompilerError> {
        let f = &ws.module(decl.module).functions[decl.func];
        let Some((bone_core::hir::FnBackend::Js, body)) = f.inline_body else {
            return Ok(Vec::new());
        };
        if !body.as_str().contains(TABLE) {
            return Ok(Vec::new());
        }
        self.used = true;
        let mut factories = BTreeSet::new();
        for &arg in args {
            if let Some(root) = self.interner.get(arg).nominal() {
                self.reach(ws, root, &mut factories)?;
            }
        }
        let by_name = json_factory_refs(ws);
        Ok(factories
            .into_iter()
            .filter_map(|name| by_name.get(&name).copied())
            .collect())
    }

    fn reach(
        &mut self,
        ws: &Workspace,
        root: Ustr,
        factories: &mut BTreeSet<String>,
    ) -> Result<(), CompilerError> {
        let symbols = symbol_keys(ws);
        let table = skull_mir::type_table::json_field_factories(ws);
        let arena = Arc::clone(&self.arena);
        let is_gc = |name| {
            arena
                .nominal_value_storage_required(ws, bone_core::hir::FnBackend::Js, name)
                .is_ok_and(|storage| storage != ValueStorage::CData)
        };
        let mut work = vec![root];
        while let Some(key) = work.pop() {
            if self.entries.contains_key(&key) {
                continue;
            }
            // A key can name both a stdlib struct and a user enum. Lua writes
            // structs, then newtypes, then enums into one table keyed by
            // symbol, so the later kind wins; resolve in that priority.
            let entries = if let Some(eid) = ws.enums_by_name.get(&key) {
                skull_mir::type_table::enum_entries(ws, &ws.module(eid.module).enums[eid.def])
            } else if ws.newtypes_by_name.contains_key(&key) {
                vec![Entry::Newtype {
                    name: skull_mir::mangle::type_symbol(ws, key),
                }]
            } else if let Some(sid) = ws.struct_by_name.get(&key) {
                let s = &ws.module(sid.module).structs[sid.def];
                skull_mir::type_table::struct_entries(
                    ws,
                    bone_core::hir::FnBackend::Js,
                    &self.arena,
                    &is_gc,
                    &table,
                    s,
                )?
            } else {
                Vec::new()
            };
            for entry in &entries {
                if let Entry::Struct { fields, .. } = entry {
                    for f in fields {
                        factories.extend(f.json_default.iter().cloned());
                        factories.extend(f.json_omit.iter().cloned());
                    }
                }
                for name in entry.referenced_types() {
                    if let Some(&next) = symbols.get(&name) {
                        work.push(next);
                    }
                }
            }
            self.entries.insert(key, entries);
        }
        Ok(())
    }

    pub(super) fn render(&self) -> String {
        if !self.used {
            return String::new();
        }
        let mut types = Vec::new();
        let mut flat = Vec::new();
        let mut strided = Vec::new();
        for entry in self.entries.values().flatten() {
            match entry {
                Entry::StridedRows { name, paths } => {
                    let offsets = paths
                        .iter()
                        .enumerate()
                        .map(|(i, path)| format!("{}: {i}", js_string_lit(path.as_str())))
                        .collect::<Vec<_>>();
                    strided.push(format!(
                        "{}: {{{}}}",
                        js_string_lit(name.as_str()),
                        offsets.join(", ")
                    ));
                }
                Entry::FlatLayout {
                    name, tuple_width, ..
                } => flat.push(format!(
                    "{}: {{tuple_width: {tuple_width}}}",
                    js_string_lit(name.as_str())
                )),
                Entry::Struct {
                    name,
                    kind,
                    fields,
                    vec_columns,
                } => {
                    let fields = fields.iter().map(field_js).collect::<Vec<_>>();
                    let columns = match vec_columns {
                        Some(paths) => format!(", vec_columns: {}", strings_js(paths)),
                        None => String::new(),
                    };
                    types.push(format!(
                        "{}: {{kind: \"{kind}\", fields: [{}]{columns}}}",
                        js_string_lit(name.as_str()),
                        fields.join(", ")
                    ));
                }
                Entry::Newtype { name } => types.push(format!(
                    "{}: {{kind: \"newtype\", fields: []}}",
                    js_string_lit(name.as_str())
                )),
                Entry::EnumTag { name, variants } => types.push(format!(
                    "{}: {{kind: \"enum_tag\", variants: {}}}",
                    js_string_lit(name.as_str()),
                    strings_js(variants)
                )),
                Entry::Enum { name, variants } => {
                    let variants = variants
                        .iter()
                        .map(|(variant, entry)| {
                            format!("{}: {}", js_string_lit(variant.as_str()), variant_js(entry))
                        })
                        .collect::<Vec<_>>();
                    types.push(format!(
                        "{}: {{kind: \"enum\", variant_desc: {{{}}}}}",
                        js_string_lit(name.as_str()),
                        variants.join(", ")
                    ));
                }
                Entry::CEnum {
                    name,
                    slots,
                    variants,
                } => {
                    let slots = slots.iter().map(|(slot, _)| *slot).collect::<Vec<_>>();
                    let variants = variants
                        .iter()
                        .map(|(variant, payload)| {
                            let fields = payload
                                .iter()
                                .map(|p| {
                                    format!(
                                        "{{name: {}, slot: {}, type: {}}}",
                                        js_string_lit(&p.name),
                                        js_string_lit(&p.slot),
                                        js_string_lit(&p.ty)
                                    )
                                })
                                .collect::<Vec<_>>();
                            format!(
                                "{{name: {}, fields: [{}]}}",
                                js_string_lit(variant.as_str()),
                                fields.join(", ")
                            )
                        })
                        .collect::<Vec<_>>();
                    types.push(format!(
                        "{}: {{kind: \"c_enum\", slots: {}, variants: [{}]}}",
                        js_string_lit(name.as_str()),
                        strings_js(&slots),
                        variants.join(", ")
                    ));
                }
            }
        }
        format!(
            "var {TABLE} = {{\ntypes: {{\n{}\n}},\nflat: {{{}}},\nstrided: {{{}}}\n}};\n",
            types.join(",\n"),
            flat.join(", "),
            strided.join(", ")
        )
    }
}

/// Every declared nominal's emitted type symbol, mapped back to its key.
fn symbol_keys(ws: &Workspace) -> rustc_hash::FxHashMap<Ustr, Ustr> {
    ws.struct_by_name
        .keys()
        .chain(ws.newtypes_by_name.keys())
        .chain(ws.enums_by_name.keys())
        .map(|&key| (skull_mir::mangle::type_symbol(ws, key), key))
        .collect()
}

fn json_factory_refs(ws: &Workspace) -> rustc_hash::FxHashMap<String, FuncRef> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| f.json_field_for.is_some())
                .map(move |(fid, f)| {
                    (
                        skull_mir::mangle::mangle_fn(ws, f),
                        FuncRef {
                            module: m.id,
                            func: fid,
                        },
                    )
                })
        })
        .collect()
}

fn strings_js(items: &[Ustr]) -> String {
    let items = items
        .iter()
        .map(|item| js_string_lit(item.as_str()))
        .collect::<Vec<_>>();
    format!("[{}]", items.join(", "))
}

fn shape_js(shape: &Shape, extra: &str) -> String {
    match shape {
        Shape::Prim(name) => format!("{{k: \"{name}\"{extra}}}"),
        Shape::Opt(inner) => format!("{{k: \"opt\", inner: {}{extra}}}", shape_js(inner, "")),
        Shape::Tuple(elems) => {
            let elems = elems.iter().map(|e| shape_js(e, "")).collect::<Vec<_>>();
            format!("{{k: \"tuple\", elems: [{}]{extra}}}", elems.join(", "))
        }
        Shape::Vec(elem) => format!("{{k: \"vec\", elem: {}{extra}}}", shape_js(elem, "")),
        Shape::Map(key, val) => format!(
            "{{k: \"map\", key: {}, val: {}{extra}}}",
            shape_js(key, ""),
            shape_js(val, "")
        ),
        Shape::Field(inner) => {
            format!("{{k: \"field\", inner: {}{extra}}}", shape_js(inner, ""))
        }
        Shape::Nom(name) => format!(
            "{{k: \"nom\", name: {}{extra}}}",
            js_string_lit(name.as_str())
        ),
    }
}

fn variant_js(entry: &VariantEntry) -> String {
    match entry {
        VariantEntry::Positional(positions) => {
            let positions = positions
                .iter()
                .map(|p| {
                    let mut extra = String::new();
                    if let Some(slot) = p.slot {
                        extra.push_str(&format!(", slot: {slot}"));
                    }
                    if let Some(leaves) = &p.leaves {
                        extra.push_str(&format!(", leaves: {}", strings_js(leaves)));
                    }
                    shape_js(p.shape.as_ref().unwrap_or(&Shape::Prim("any")), &extra)
                })
                .collect::<Vec<_>>();
            format!("{{pos: [{}]}}", positions.join(", "))
        }
        VariantEntry::Named(fields) => {
            let fields = fields
                .iter()
                .map(|(name, shape)| {
                    format!("{}: {}", js_string_lit(name.as_str()), shape_js(shape, ""))
                })
                .collect::<Vec<_>>();
            format!("{{named: {{{}}}}}", fields.join(", "))
        }
    }
}

fn field_js(f: &FieldEntry) -> String {
    let mut out = format!(
        "{{name: {}, type: {}, nullable: {}",
        js_string_lit(f.name.as_str()),
        js_string_lit(&f.ty),
        f.nullable
    );
    if f.embed {
        out.push_str(", embed: true");
    }
    if let Some(keys) = &f.flat {
        out.push_str(&format!(", flat: {}", strings_js(keys)));
    }
    if let Some(shape) = &f.shape {
        out.push_str(&format!(", desc: {}", shape_js(shape, "")));
    }
    if let Some(name) = f.json_name {
        out.push_str(&format!(", json_name: {}", js_string_lit(name.as_str())));
    }
    if let Some(factory) = &f.json_default {
        out.push_str(&format!(", json_default: {factory}"));
    }
    if let Some(predicate) = &f.json_omit {
        out.push_str(&format!(", json_omit: {predicate}"));
    }
    if f.json_skip {
        out.push_str(", json_skip: true");
    }
    out.push('}');
    out
}
