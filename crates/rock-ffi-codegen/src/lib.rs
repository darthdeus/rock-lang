//! Build-time generation of Rock bindings and C prototypes from Rust exports.

use bone_semantic::prelude::{Ustr, Workspace};
use skull_mir::lower::{CFieldType, value_c_layout};
use skull_mir::mangle::type_symbol;
use skull_parser::parse_module_clean;
use std::collections::BTreeMap;
use std::fmt::Write;
use syn::parse::{ParseStream, Parser};
use syn::{
    Attribute, Expr, FnArg, ItemType, Lit, Meta, Pat, PointerMutability, ReturnType, Signature,
    Token, Type, Visibility, token,
};

pub struct Layout {
    pub rock: &'static str,
    pub rust: &'static str,
}

pub struct Module {
    pub package: &'static str,
    pub library: &'static str,
    pub layouts: &'static [Layout],
    pub opaque_pointers: &'static [(&'static str, &'static str)],
}

#[derive(Default)]
pub struct Bindings {
    pub cdef: String,
    pub rock: String,
    pub rust: String,
}

impl Module {
    /// Each source is paired with its Rust module path relative to the crate root.
    /// Exports opt in with a `rock-ffi <package>` doc comment.
    /// `string-len` projects `(*const c_char, usize) -> c_int` to one Rock
    /// string argument, preserving embedded NULs. `load=<name>` on that
    /// projection calls a Lua-visible loader before the native call.
    /// Add `nomangle` if inline Lua also calls the generated wrapper by name.
    pub fn bindings<'a>(
        &self,
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Bindings, String> {
        let mut bindings = Bindings::default();
        for layout in self
            .layouts
            .iter()
            .filter(|layout| layout.rock != layout.rust)
        {
            writeln!(bindings.cdef, "typedef {} {};", layout.rock, layout.rust).unwrap();
        }
        let mut exports = BTreeMap::new();
        let mut arrays = BTreeMap::new();
        for (module, source) in sources {
            if !source.contains("rock-ffi") {
                continue;
            }
            let parse = |input: ParseStream<'_>| {
                while !input.is_empty() {
                    if !(input.peek(Token![#]) && input.peek2(token::Bracket)) {
                        skip_token(input)?;
                        continue;
                    }
                    let attrs = input.call(Attribute::parse_outer)?;
                    let mark =
                        |key| marker(&attrs, key, self.package).map_err(|error| input.error(error));
                    if let Some(options) = mark("rock-ffi")? {
                        let _: Visibility = input.parse()?;
                        let sig: Signature = input.parse()?;
                        let name = sig.ident.to_string();
                        if !attrs.iter().any(|attr| attr.path().is_ident("no_mangle"))
                            || !input.peek(token::Brace)
                        {
                            return Err(input
                                .error(format!("{name}: expected a fixed, unmangled C export")));
                        }
                        skip_token(input)?;
                        if exports
                            .insert(name.clone(), (module, sig, options))
                            .is_some()
                        {
                            return Err(input.error(format!("{name}: duplicate FFI export")));
                        }
                    } else if let Some(prefix) = mark("rock-ffi-array")? {
                        let alias: ItemType = input.parse()?;
                        if arrays.insert(prefix.clone(), alias.ty).is_some() {
                            return Err(input.error(format!("{prefix}: duplicate packed array")));
                        }
                    }
                }
                Ok(())
            };
            parse
                .parse_str(source)
                .map_err(|e| format!("{module}: {e}"))?;
        }
        for (name, (module, sig, options)) in exports {
            self.emit_export(&mut bindings, &name, module, &sig, &options)
                .map_err(|e| format!("{name}: {e}"))?;
        }
        for (prefix, array) in arrays {
            self.emit_array(&mut bindings.rock, &prefix, &array)
                .map_err(|e| format!("{prefix}: {e}"))?;
        }
        Ok(bindings)
    }

    fn emit_array(&self, out: &mut String, prefix: &str, ty: &Type) -> Result<(), String> {
        let Type::Array(array) = ty else {
            return Err("expected [element; count]".into());
        };
        let Expr::Lit(length) = &array.len else {
            return Err("expected a literal count".into());
        };
        let Lit::Int(length) = &length.lit else {
            return Err("expected an integer count".into());
        };
        let count = length.base10_parse::<usize>().map_err(|e| e.to_string())?;
        if count == 0 || prefix.is_empty() {
            return Err("expected a named, nonempty packed array".into());
        }
        let elem = rust_name(&array.elem)?;
        let (c, bytes) =
            if let Some(layout) = self.layouts.iter().find(|layout| layout.rust == elem) {
                (layout.rust, None)
            } else {
                let scalar = scalar(&elem)?;
                (scalar.c, scalar.bytes)
            };
        writeln!(out, "\npub const {prefix}_LANES: int = {count}").unwrap();
        writeln!(out, "pub const {prefix}_POINTER: string = \"{c}*\"").unwrap();
        if let Some(bytes) = bytes {
            let stride = count.checked_mul(bytes).ok_or("packed stride overflow")?;
            writeln!(out, "pub const {prefix}_BYTES: int = {stride}").unwrap();
        }
        Ok(())
    }

    fn emit_export(
        &self,
        out: &mut Bindings,
        name: &str,
        module: &str,
        sig: &Signature,
        options: &str,
    ) -> Result<(), String> {
        let mut options = ExportOptions::parse(options)?;
        if sig
            .abi
            .as_ref()
            .and_then(|abi| abi.name.as_ref())
            .is_none_or(|abi| abi.value() != "C")
            || sig.variadic.is_some()
            || !sig.generics.params.is_empty()
        {
            return Err("expected a fixed, unmangled C export".into());
        }
        let mut c_params = Vec::new();
        let mut rust_params = Vec::new();
        let mut rock_params = Vec::new();
        let mut out_value = None;
        let return_out = options.take("return-out").is_some();
        let string_len = options.take("string-len").is_some();
        let mut string_param = None;
        if string_len && (return_out || sig.inputs.len() != 2) {
            return Err("string-len requires (const char*, size_t) -> int".into());
        }
        for (index, param) in sig.inputs.iter().enumerate() {
            let FnArg::Typed(param) = param else {
                return Err("receiver in C export".into());
            };
            let Pat::Ident(param_name) = param.pat.as_ref() else {
                return Err("expected a named parameter".into());
            };
            let name = param_name.ident.to_string();
            let mut wire = self
                .wire_type(&param.ty)
                .map_err(|e| format!("{name}: {e}"))?;
            c_params.push(format!("{} {name}", wire.c));
            if index == 0 && return_out {
                if !matches!(param.ty.as_ref(), Type::Ptr(ptr) if matches!(ptr.mutability, PointerMutability::Mut(_)))
                {
                    return Err("return-out needs a leading mutable pointer".into());
                }
                out_value = self
                    .layouts
                    .iter()
                    .find(|layout| wire.c == format!("{}*", layout.rust));
                if out_value.is_none() {
                    return Err("return-out needs a generated value layout".into());
                }
            } else if string_len && index == 1 {
                if wire.rust != "usize" {
                    return Err("string-len requires (const char*, size_t) -> int".into());
                }
            } else {
                options.project(&name, &mut wire)?;
                let rock_name = options.take(&format!("rename:{name}")).unwrap_or(&name);
                let default = options
                    .take(&format!("default:{name}"))
                    .map_or_else(String::new, |value| format!(" = {value}"));
                if string_len {
                    if wire.c != "const char*" || wire.rock != "string" {
                        return Err("string-len requires (const char*, size_t) -> int".into());
                    }
                    string_param = Some(rock_name.to_owned());
                }
                rock_params.push(format!("{rock_name}: {}{default}", wire.rock));
            }
            rust_params.push(wire.rust);
        }
        let mut ret = match &sig.output {
            ReturnType::Default => Wire::new("void", "()", ""),
            ReturnType::Type(_, ty) => self.wire_type(ty)?,
        };
        // The Rock bool wrapper compares the result against the number zero.
        let c_bool = options.take("c-bool").is_some();
        if c_bool && ret.rust != "bool" {
            return Err("c-bool requires a bool return".into());
        }
        if ret.rust == "bool" && !c_bool {
            ret.c = "uint8_t".into();
        }
        if options.take("byte-bool").is_some() {
            if ret.rust != "u8" {
                return Err("byte-bool requires a u8 return".into());
            }
            ret.rock = "bool".into();
        }
        if return_out {
            let layout = out_value
                .filter(|_| ret.rust == "()")
                .ok_or("return-out needs a leading writable value slot and a void return")?;
            ret.rock = layout.rock.into();
        }
        options.project("return", &mut ret)?;
        let visibility = if options.take("private").is_some() {
            ""
        } else {
            "pub "
        };
        let loader = if string_len {
            if ret.c != "int" || ret.rock != "int" {
                return Err("string-len requires (const char*, size_t) -> int".into());
            }
            options
                .take("load")
                .map(|loader| {
                    syn::parse_str::<syn::Ident>(loader)
                        .map_err(|_| "load requires the Lua name of a loader function")
                })
                .transpose()?
        } else {
            None
        };
        let nomangle = if string_len && options.take("nomangle").is_some() {
            "@nomangle "
        } else {
            ""
        };
        if let Some(option) = options.0.keys().next() {
            return Err(format!("unused FFI option `{option}`"));
        }
        if c_params.is_empty() {
            c_params.push("void".into());
        }
        writeln!(out.cdef, "{} {name}({});", ret.c, c_params.join(", ")).unwrap();
        let rock_ret = if ret.rock.is_empty() {
            String::new()
        } else {
            format!(" -> {}", ret.rock)
        };
        let params = rock_params.join(", ");
        if let Some(data) = string_param {
            // Preserve embedded NULs and keep the native call in the Lua wrapper.
            writeln!(
                out.rock,
                "{visibility}{nomangle}inline(lua) fn {name}({params}){rock_ret} = \"\"\""
            )
            .unwrap();
            if let Some(loader) = loader {
                writeln!(out.rock, "    {loader}()").unwrap();
            }
            writeln!(
                out.rock,
                "    return tonumber({}.{name}({data}, #{data}))\n\"\"\"",
                self.library
            )
            .unwrap();
        } else {
            writeln!(
                out.rock,
                "{visibility}extern(ffi, \"{}\") fn {name}({params}){rock_ret}",
                self.library
            )
            .unwrap();
        }
        let path = if module.is_empty() {
            "crate".into()
        } else {
            format!("crate::{module}")
        };
        writeln!(
            out.rust,
            "const _: unsafe extern \"C\" fn({}) -> {} = {path}::{name};",
            rust_params.join(", "),
            ret.rust
        )
        .unwrap();
        Ok(())
    }

    fn wire_type(&self, ty: &Type) -> Result<Wire, String> {
        if let Type::Ptr(ptr) = ty {
            let elem = rust_name(&ptr.elem)?;
            let (c, rock, rust) =
                if let Some(layout) = self.layouts.iter().find(|layout| layout.rust == elem) {
                    (layout.rust, format!("Ptr<{}>", layout.rock), layout.rust)
                } else if let Some(&(name, rust)) =
                    self.opaque_pointers.iter().find(|&&(name, _)| name == elem)
                {
                    (name, name.into(), rust)
                } else {
                    let wire = scalar(&elem)?;
                    let rock = match elem.as_str() {
                        "u8" => "PtrU8".into(),
                        "u32" => "PtrU32".into(),
                        "i32" | "c_int" | "usize" | "isize" => "Ptr<int>".into(),
                        "f32" | "c_float" => "Ptr<float>".into(),
                        "c_void" => "PtrU8".into(),
                        "bool" => "Ptr<bool>".into(),
                        "c_char" if matches!(ptr.mutability, PointerMutability::Const(_)) => {
                            "string".into()
                        }
                        _ => return Err(format!("no Rock pointer mapping for `{elem}`")),
                    };
                    (wire.c, rock, canonical_rust(&elem))
                };
            let (qualifier, mutability) = if matches!(ptr.mutability, PointerMutability::Mut(_)) {
                ("", "mut")
            } else {
                ("const ", "const")
            };
            return Ok(Wire {
                c: format!("{qualifier}{c}*"),
                rust: format!("*{mutability} {rust}"),
                rock,
            });
        }
        let name = rust_name(ty)?;
        let wire = scalar(&name)?;
        Ok(Wire::new(wire.c, canonical_rust(&name), wire.rock))
    }

    /// Generate Rust wire types and LuaJIT layout tests from Rock value declarations.
    pub fn layouts(&self, file: &str, source: &str) -> Result<(String, String), String> {
        let mut ast = [parse_module_clean(source, Ustr::from(file)).map_err(|e| e.message)?];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut ast).map_err(|e| e.message)?;
        let mut rust = String::new();
        let mut tests = String::new();
        for layout in self.layouts {
            emit_layout(&ws, layout, &mut rust, &mut tests)?;
        }
        Ok((rust, tests))
    }
}

// Rust checks the bodies. The generator only needs top-level attributes and signatures.
fn skip_token(input: ParseStream<'_>) -> syn::Result<()> {
    input.step(|cursor| {
        cursor
            .token_tree()
            .map(|(_, rest)| ((), rest))
            .ok_or_else(|| cursor.error("expected a Rust token"))
    })
}

fn marker(attrs: &[Attribute], key: &str, package: &str) -> Result<Option<String>, String> {
    let mut marker = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("doc")) {
        let Meta::NameValue(doc) = &attr.meta else {
            continue;
        };
        let Expr::Lit(lit) = &doc.value else { continue };
        let Lit::Str(line) = &lit.lit else { continue };
        let line = line.value();
        let mut words = line.trim().splitn(3, ' ');
        if words.next() != Some(key) || words.next() != Some(package) {
            continue;
        }
        if marker
            .replace(words.next().unwrap_or_default().into())
            .is_some()
        {
            return Err(format!("duplicate {key} marker for {package}"));
        }
    }
    Ok(marker)
}

fn rust_name(ty: &Type) -> Result<String, String> {
    if let Type::Path(path) = ty {
        if let Some(name) = path.path.get_ident() {
            return Ok(name.to_string());
        }
    }
    Err("unsupported Rust FFI type".into())
}

fn canonical_rust(name: &str) -> &str {
    match name {
        "int" => "i64",
        "c_int" => "::std::ffi::c_int",
        "c_uint" => "::std::ffi::c_uint",
        "c_long" => "::std::ffi::c_long",
        "c_ulong" => "::std::ffi::c_ulong",
        "c_char" => "::std::ffi::c_char",
        "c_void" => "::std::ffi::c_void",
        "c_float" => "f32",
        "c_double" => "f64",
        _ => name,
    }
}

struct Scalar {
    c: &'static str,
    rock: &'static str,
    bytes: Option<usize>,
}

fn scalar(rust: &str) -> Result<Scalar, String> {
    let (c, rock, bytes) = match rust {
        "i32" | "c_int" => ("int", "int", Some(4)),
        "u32" => ("uint32_t", "int", Some(4)),
        "c_uint" => ("unsigned int", "int", Some(4)),
        "i64" | "int" => ("int64_t", "int", Some(8)),
        "u64" => ("uint64_t", "int", Some(8)),
        "usize" => ("size_t", "int", None),
        "isize" => ("intptr_t", "int", None),
        "c_long" => ("long", "int", None),
        "c_ulong" => ("unsigned long", "int", None),
        "u8" => ("uint8_t", "int", Some(1)),
        "bool" => ("bool", "bool", Some(1)),
        "f32" | "c_float" => ("float", "float", Some(4)),
        "f64" | "c_double" => ("double", "float", Some(8)),
        "c_char" => ("char", "int", Some(1)),
        "c_void" => ("void", "", None),
        _ => return Err(format!("unsupported Rust FFI type `{rust}`")),
    };
    Ok(Scalar { c, rock, bytes })
}

struct ExportOptions<'a>(BTreeMap<&'a str, &'a str>);

impl<'a> ExportOptions<'a> {
    fn parse(options: &'a str) -> Result<Self, String> {
        let mut parsed = BTreeMap::new();
        for option in options.split_whitespace() {
            let (key, value) = option.split_once('=').unwrap_or((option, ""));
            if parsed.insert(key, value).is_some() {
                return Err(format!("duplicate FFI option `{key}`"));
            }
        }
        Ok(Self(parsed))
    }

    fn take(&mut self, key: &str) -> Option<&'a str> {
        self.0.remove(key)
    }

    fn project(&mut self, name: &str, wire: &mut Wire) -> Result<(), String> {
        if let Some(view) = self.take(&format!("view:{name}")) {
            if !matches!(wire.c.as_str(), "void*" | "const void*") {
                return Err(format!("{name}: view requires a void pointer"));
            }
            wire.rock = format!("Ptr<{view}>");
        }
        if self.take(&format!("nullable:{name}")).is_some() {
            if wire.c != "const char*" {
                return Err(format!("{name}: nullable requires a const C string"));
            }
            wire.rock = "string?".into();
        }
        if self.take(&format!("uint:{name}")).is_some() {
            if !matches!(
                wire.rust.as_str(),
                "u8" | "u32" | "u64" | "usize" | "::std::ffi::c_uint" | "::std::ffi::c_ulong"
            ) || wire.rock != "int"
            {
                return Err(format!("{name}: uint requires an unsigned integer"));
            }
            wire.rock = "uint".into();
        }
        Ok(())
    }
}

struct Wire {
    c: String,
    rust: String,
    rock: String,
}

impl Wire {
    fn new(c: &str, rust: &str, rock: &str) -> Self {
        Self {
            c: c.into(),
            rust: rust.into(),
            rock: rock.into(),
        }
    }
}

fn emit_layout(
    ws: &Workspace,
    layout: &Layout,
    rust: &mut String,
    tests: &mut String,
) -> Result<(), String> {
    let Layout { rock, rust: ty } = *layout;
    let name = ws
        .source_modules()
        .flat_map(|m| m.structs.iter().map(|(_, s)| s.name))
        .find(|name| type_symbol(ws, *name) == rock)
        .ok_or_else(|| format!("{rock}: missing shared Rock value with @nomangle"))?;
    let layout = value_c_layout(ws, name)
        .filter(|layout| !layout.fields.is_empty())
        .ok_or_else(|| format!("{rock}: expected a nonempty C value layout"))?;
    writeln!(
        rust,
        "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct {ty} {{"
    )
    .unwrap();
    for (field, _, field_ty) in &layout.fields {
        let field_ty = match field_ty {
            CFieldType::Scalar("int") => "i32",
            CFieldType::Scalar("uint32_t") => "u32",
            CFieldType::Scalar("double") => "f64",
            CFieldType::Scalar("float") => "f32",
            CFieldType::Scalar("bool") => "bool",
            _ => return Err(format!("{rock}.{field}: unsupported C field type")),
        };
        writeln!(rust, "    pub r#{field}: {field_ty},").unwrap();
    }
    writeln!(rust, "}}\nconst _: () = {{").unwrap();
    let (size, align) = (layout.size, layout.align);
    writeln!(
        rust,
        "    assert!(::std::mem::size_of::<{ty}>() == {size});"
    )
    .unwrap();
    writeln!(
        rust,
        "    assert!(::std::mem::align_of::<{ty}>() == {align});"
    )
    .unwrap();

    writeln!(tests, "\n@test inline(lua) fn ffi_layout_{rock}() = \"\"\"").unwrap();
    writeln!(
        tests,
        "    assert(ffi.sizeof('{rock}') == {size}, '{rock} size')"
    )
    .unwrap();
    writeln!(
        tests,
        "    assert(ffi.alignof('{rock}') == {align}, '{rock} alignment')"
    )
    .unwrap();
    for (field, offset, _) in layout.fields {
        writeln!(
            rust,
            "    assert!(::std::mem::offset_of!({ty}, r#{field}) == {offset});"
        )
        .unwrap();
        writeln!(
            tests,
            "    assert(ffi.offsetof('{rock}', '{field}') == {offset}, '{rock}.{field} offset')"
        )
        .unwrap();
    }
    writeln!(rust, "}};").unwrap();
    writeln!(tests, "\"\"\"").unwrap();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE: Module = Module {
        package: "example-native",
        library: "example",
        layouts: &[],
        opaque_pointers: &[],
    };

    #[test]
    fn byte_string_keeps_the_c_abi_and_projects_the_length_in_lua() {
        let source = r#"
            /// rock-ffi example-native string-len load=example_load nomangle
            #[no_mangle]
            pub extern "C" fn bytes(data: *const c_char, len: usize) -> c_int { 0 }
        "#;
        let bindings = NATIVE.bindings([("", source)]).unwrap();
        assert_eq!(bindings.cdef, "int bytes(const char* data, size_t len);\n");
        assert_eq!(
            bindings.rock,
            "pub @nomangle inline(lua) fn bytes(data: string) -> int = \"\"\"\n    example_load()\n    return tonumber(example.bytes(data, #data))\n\"\"\"\n"
        );
        assert_eq!(
            bindings.rust,
            "const _: unsafe extern \"C\" fn(*const ::std::ffi::c_char, usize) -> ::std::ffi::c_int = crate::bytes;\n"
        );
        let source = source.replace("len: usize", "len: u32");
        let error = NATIVE.bindings([("", source.as_str())]).err().unwrap();
        assert!(error.contains("string-len requires"), "{error}");
    }
}
