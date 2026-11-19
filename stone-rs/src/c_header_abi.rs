use std::ffi::{c_char, c_float, c_int, c_void};
use std::path::Path;
use std::process::Command;

#[derive(Clone)]
enum CType {
    Base(&'static str),
    Const(Box<CType>),
    Pointer(Box<CType>),
}

impl CType {
    fn declaration(&self, name: &str) -> String {
        match self {
            Self::Base(base) => format!("{base} {name}"),
            Self::Const(inner) => match inner.as_ref() {
                Self::Base(base) => format!("const {base} {name}"),
                Self::Pointer(pointee) => pointee.declaration(&format!("* const {name}")),
                Self::Const(nested) => Self::Const(nested.clone()).declaration(name),
            },
            Self::Pointer(pointee) => pointee.declaration(&format!("*{name}")),
        }
    }

    fn spelling(&self) -> String {
        self.declaration("").trim().to_owned()
    }
}

trait CAbiType {
    fn c_type() -> CType;
}

macro_rules! impl_c_abi_type {
    ($rust_type:ty => $c_type:literal) => {
        impl CAbiType for $rust_type {
            fn c_type() -> CType {
                CType::Base($c_type)
            }
        }
    };
}

impl_c_abi_type!(() => "void");
impl_c_abi_type!(bool => "bool");
impl_c_abi_type!(c_char => "char");
impl_c_abi_type!(c_int => "int");
impl_c_abi_type!(c_float => "float");
impl_c_abi_type!(f64 => "double");
impl_c_abi_type!(u32 => "uint32_t");
impl_c_abi_type!(u64 => "uint64_t");
impl_c_abi_type!(i64 => "int64_t");
impl_c_abi_type!(usize => "size_t");
impl_c_abi_type!(c_void => "void");

impl CAbiType for crate::StoneWindow {
    fn c_type() -> CType {
        CType::Base("StoneWindow")
    }
}

impl CAbiType for crate::StoneString {
    fn c_type() -> CType {
        CType::Base("StoneString")
    }
}

impl CAbiType for crate::GraphicsStats {
    fn c_type() -> CType {
        CType::Base("StoneGraphicsStats")
    }
}

impl CAbiType for crate::image_texture::ImageLoadStats {
    fn c_type() -> CType {
        CType::Base("StoneImageLoadStats")
    }
}

impl<T: CAbiType> CAbiType for *const T {
    fn c_type() -> CType {
        CType::Pointer(Box::new(CType::Const(Box::new(T::c_type()))))
    }
}

impl<T: CAbiType> CAbiType for *mut T {
    fn c_type() -> CType {
        CType::Pointer(Box::new(T::c_type()))
    }
}

trait CAbiFunction {
    fn append_assertion(name: &str, assertions: &mut String);
}

macro_rules! impl_c_abi_function {
    ($($arg:ident),*) => {
        impl<R, $($arg,)*> CAbiFunction for extern "C" fn($($arg),*) -> R
        where
            R: CAbiType,
            $($arg: CAbiType,)*
        {
            fn append_assertion(name: &str, assertions: &mut String) {
                let args: Vec<String> = vec![$(<$arg as CAbiType>::c_type().spelling()),*];
                let args = if args.is_empty() {
                    "void".to_owned()
                } else {
                    args.join(", ")
                };
                let result = <R as CAbiType>::c_type().spelling();
                assertions.push_str(&format!("ABI({name}, {result} (*)({args}));\n"));
            }
        }
    };
}

impl_c_abi_function!();
impl_c_abi_function!(A0);
impl_c_abi_function!(A0, A1);
impl_c_abi_function!(A0, A1, A2);
impl_c_abi_function!(A0, A1, A2, A3);
impl_c_abi_function!(A0, A1, A2, A3, A4);
impl_c_abi_function!(A0, A1, A2, A3, A4, A5);
impl_c_abi_function!(A0, A1, A2, A3, A4, A5, A6);
impl_c_abi_function!(A0, A1, A2, A3, A4, A5, A6, A7);
impl_c_abi_function!(A0, A1, A2, A3, A4, A5, A6, A7, A8);
impl_c_abi_function!(A0, A1, A2, A3, A4, A5, A6, A7, A8, A9);

fn append_assertion<F: CAbiFunction>(name: &str, _: F, assertions: &mut String) {
    F::append_assertion(name, assertions);
}

#[test]
fn c_header_function_signatures_match_rust_exports() {
    let mut assertions = String::new();
    macro_rules! abi {
        ($function:path $(, $arg:ty)*) => {
            append_assertion(
                stringify!($function).rsplit("::").next().unwrap().trim(),
                $function as extern "C" fn($($arg),*) -> _,
                &mut assertions,
            );
        };
    }

    abi!(crate::stone_init);
    abi!(crate::stone_teardown);
    abi!(crate::stone_create_window_ex, _, _, _, _, _, _, _, _);
    abi!(crate::stone_get_window_size, _, _);
    abi!(crate::stone_get_drawable_size, _, _);
    abi!(crate::stone_get_graphics_stats, _);
    abi!(crate::stone_track_text_draw_call);
    abi!(crate::image_texture::stone_get_image_frame_stats, _);
    abi!(crate::image_texture::stone_get_image_profile_stats, _);
    abi!(crate::stone_get_dpi_scale);
    abi!(crate::stone_get_config_imgui_scale);
    abi!(crate::canvas::stone_clear_canvas, _, _, _, _);
    abi!(crate::canvas::stone_clear_canvas_int, _, _);
    abi!(crate::canvas::stone_canvas_attach_id_target, _, _);
    abi!(crate::image_texture::stone_get_white_image);
    abi!(crate::stone_swap_window, _);
    abi!(crate::stone_delay, _);
    abi!(crate::filesystem::stone_get_pref_path, _, _);
    abi!(crate::input::stone_scancode_count);
    abi!(crate::input::stone_scancode_token_at, _);
    abi!(crate::input::stone_scancode_token_for, _);
    abi!(crate::input::stone_scancode_value, _);
    abi!(crate::input::stone_scancode_label, _);
    abi!(crate::filesystem::stone_file_read, _, _);
    abi!(crate::filesystem::stone_free, _);
    abi!(crate::filesystem::stone_file_append, _, _, _);
    abi!(crate::telemetry::stone_telemetry_clear, _);
    abi!(
        crate::telemetry::stone_telemetry_configure,
        _,
        _,
        _,
        _,
        _,
        _,
        _,
        _
    );
    abi!(crate::text::stone_get_font_instance_image, _);
    abi!(crate::image_texture::stone_supports_bc1);

    let dir = tempfile::tempdir().expect("temporary C ABI fixture directory");
    let source_path = dir.path().join("rust_abi.c");
    let source = format!(
        "#include <stone.h>\n\
         #define ABI(name, signature) _Static_assert(\
             __builtin_types_compatible_p(__typeof__(&name), signature), #name \" ABI\")\n\
         {assertions}"
    );
    std::fs::write(&source_path, source).expect("write C ABI fixture");

    let result = Command::new("clang")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fsyntax-only"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&source_path)
        .output()
        .expect("run clang");
    assert!(
        result.status.success(),
        "stone.h signatures disagree with Rust exports:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
