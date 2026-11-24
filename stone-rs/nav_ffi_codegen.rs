use bone_semantic::{
    hir::Function,
    prelude::{SourceFiles, Ustr, Workspace},
};
use skull_mir::lower::{value_c_fields, CFieldType};
use skull_mir::mangle::{declared_name, mangle_fn, type_symbol};
use skull_mir::monoty::{convert_decl, MonoCtor, MonoSubst, MonoTy};
use skull_parser::{parse_module_clean, stdlib::ModuleSet};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;
use syn::{FnArg, Item, PointerMutability, ReturnType, Type};

const PREFIX: &str = "stone_nav_";
const ROCK_VALUE: &str = "NativeNavInt";
const RUST_VALUE: &str = "NativeNavIntWire";

pub struct Generated {
    pub cdef: String,
    pub rust: String,
}

pub fn load_rock(root: &Path) -> Result<SourceFiles, String> {
    let project = rock_host::compilation_units::load_project(root).map_err(|e| format!("{e:#}"))?;
    rock_host::source_files::production_only(&project.sources, root).map_err(|e| e.message)
}

pub fn generate(rock: &SourceFiles, rust: &str) -> Result<Generated, String> {
    let mut modules = ModuleSet::with_stdlib(
        rock.files
            .iter()
            .map(|source| {
                let source = source.borrow();
                let file = Ustr::from(source.file.to_id_path().as_str());
                let mut ast = parse_module_clean(&source.contents, file).map_err(|e| e.message)?;
                ast.package = source.package.clone();
                ast.global_policy = source.global_policy;
                Ok(ast)
            })
            .collect::<Result<Vec<_>, String>>()?,
    )
    .map_err(|e| e.message)?;
    let ws = bone_semantic::semantic_passes::semantic_pass_reusing_stdlib(
        &mut modules,
        rock.ns_aliases
            .as_ref()
            .ok_or("missing Stone package graph")?,
    )
    .map_err(|e| e.message)?;
    let modules = || {
        ws.source_modules()
            .filter(|m| m.package.is_some_and(|p| p == "stone_native"))
    };
    let shared = modules()
        .flat_map(|m| m.structs.iter().map(|(_, s)| s.name))
        .find(|name| type_symbol(&ws, *name) == ROCK_VALUE)
        .ok_or_else(|| format!("{ROCK_VALUE}: missing shared Rock value with @nomangle"))?;

    let mut bindings = BTreeMap::new();
    for f in modules().flat_map(|m| m.functions.iter().map(|(_, f)| f)) {
        if !declared_name(f.name.as_str()).starts_with(PREFIX) || !f.is_extern {
            continue;
        }
        let name = mangle_fn(&ws, f);
        if f.ffi_lib.is_none_or(|lib| lib != "stone") || !f.generics.is_empty() || f.is_variadic {
            return Err(format!("{name}: expected a concrete Stone FFI binding"));
        }
        if bindings.insert(name.clone(), f).is_some() {
            return Err(format!("{name}: duplicate Rock binding"));
        }
    }

    let mut generated = Generated {
        cdef: format!("typedef {ROCK_VALUE} {RUST_VALUE};\n"),
        rust: rust_layout(&ws, shared)?,
    };
    let source = syn::parse_file(rust).map_err(|e| e.to_string())?;
    for item in source.items {
        let Item::Fn(f) = item else { continue };
        let name = f.sig.ident.to_string();
        if !name.starts_with(PREFIX) {
            continue;
        }
        if !f.attrs.iter().any(|a| a.path().is_ident("no_mangle"))
            || f.sig
                .abi
                .as_ref()
                .and_then(|abi| abi.name.as_ref())
                .is_none_or(|abi| abi.value() != "C")
            || f.sig.variadic.is_some()
        {
            return Err(format!("{name}: expected a fixed, unmangled C export"));
        }
        let binding = bindings
            .remove(&name)
            .ok_or_else(|| format!("{name}: missing Rock binding"))?;
        emit_signature(&ws, binding, shared, &name, &f.sig, &mut generated)
            .map_err(|e| format!("{name}: {e}"))?;
    }
    if let Some((name, _)) = bindings.first_key_value() {
        return Err(format!("{name}: missing Rust export"));
    }
    Ok(generated)
}

fn rust_layout(ws: &Workspace, name: Ustr) -> Result<String, String> {
    let fields = value_c_fields(ws, name)
        .filter(|fields| !fields.is_empty())
        .ok_or_else(|| format!("{ROCK_VALUE}: expected a nonempty C value layout"))?;
    let mut rust = format!("#[repr(C)]\n#[derive(Clone, Copy)]\npub struct {RUST_VALUE} {{\n");
    for (field, ty) in fields {
        let ty = match ty {
            CFieldType::Scalar("int") => "i32",
            CFieldType::Scalar("uint32_t") => "u32",
            CFieldType::Scalar("double") => "f64",
            CFieldType::Scalar("float") => "f32",
            CFieldType::Scalar("bool") => "bool",
            _ => return Err(format!("{ROCK_VALUE}.{field}: unsupported C field type")),
        };
        writeln!(rust, "    r#{field}: {ty},").unwrap();
    }
    rust.push_str("}\n");
    Ok(rust)
}

struct WireType {
    c: String,
    rust: String,
    rock: MonoTy,
}

impl WireType {
    fn new(c: &str, rust: &str, rock: MonoTy) -> Self {
        Self {
            c: c.into(),
            rust: rust.into(),
            rock,
        }
    }

    fn from_rust(ty: &Type, shared: Ustr) -> Result<Self, String> {
        if let Type::Ptr(ptr) = ty {
            let (c, rust) = match ptr.elem.as_ref() {
                Type::Path(path) if path.path.is_ident("u8") => ("uint8_t", "u8"),
                Type::Path(path) if path.path.is_ident(RUST_VALUE) => (RUST_VALUE, RUST_VALUE),
                _ => return Err("unsupported pointer pointee".into()),
            };
            let nominal = |name, params| MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                params,
            };
            let rock = if rust == RUST_VALUE {
                nominal("Ptr".into(), vec![nominal(shared, vec![])])
            } else {
                nominal("PtrU8".into(), vec![])
            };
            let (qualifier, mutability) = if matches!(ptr.mutability, PointerMutability::Mut(_)) {
                ("", "mut")
            } else {
                ("const ", "const")
            };
            return Ok(Self::new(
                &format!("{qualifier}{c}*"),
                &format!("*{mutability} {rust}"),
                rock,
            ));
        }
        match ty {
            Type::Path(path) if path.path.is_ident("int") || path.path.is_ident("i64") => {
                Ok(Self::new("int64_t", "i64", MonoTy::Int))
            }
            Type::Path(path) if path.path.is_ident("f64") => {
                Ok(Self::new("double", "f64", MonoTy::Float))
            }
            _ => Err("unsupported native scalar type".into()),
        }
    }
}

fn emit_signature(
    ws: &Workspace,
    binding: &Function,
    shared: Ustr,
    name: &str,
    native: &syn::Signature,
    generated: &mut Generated,
) -> Result<(), String> {
    if binding.param_decls.len() != native.inputs.len() {
        return Err(format!(
            "Rock has {} parameters but Rust has {}",
            binding.param_decls.len(),
            native.inputs.len()
        ));
    }
    let resolve = |ty| convert_decl(ws, ty, &MonoSubst::default()).map_err(|e| format!("{e:?}"));
    let check = |slot: &str, rock: MonoTy, wire: &WireType| {
        if rock == wire.rock {
            Ok(())
        } else {
            Err(format!(
                "{slot}: Rock has {}, native {} requires {}",
                rock.render(),
                wire.rust,
                wire.rock.render()
            ))
        }
    };
    let mut c_params = Vec::new();
    let mut rust_params = Vec::new();
    for (rock, native) in binding.param_decls.iter().zip(&native.inputs) {
        let FnArg::Typed(native) = native else {
            return Err("receiver in C export".into());
        };
        let wire =
            WireType::from_rust(&native.ty, shared).map_err(|e| format!("{}: {e}", rock.name))?;
        check(rock.name.as_str(), resolve(rock.ty)?, &wire)?;
        c_params.push(format!("{} {}", wire.c, rock.name));
        rust_params.push(wire.rust);
    }
    let ret = match &native.output {
        ReturnType::Default => WireType::new("void", "()", MonoTy::Unit),
        ReturnType::Type(_, ty) => WireType::from_rust(ty, shared)?,
    };
    let rock_ret = binding
        .return_ty
        .map(resolve)
        .transpose()?
        .unwrap_or(MonoTy::Unit);
    check("return", rock_ret, &ret)?;
    if c_params.is_empty() {
        c_params.push("void".into());
    }
    writeln!(generated.cdef, "{} {name}({});", ret.c, c_params.join(", ")).unwrap();
    writeln!(
        generated.rust,
        "const _: unsafe extern \"C\" fn({}) -> {} = {name};",
        rust_params.join(", "),
        ret.rust
    )
    .unwrap();
    Ok(())
}
