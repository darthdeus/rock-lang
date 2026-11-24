#[path = "../nav_ffi_codegen.rs"]
mod nav_ffi;

const RUST: &str = include_str!("../src/nav/ffi.rs");

#[test]
fn navigation_rejects_a_same_arity_pointer_mismatch() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/stone-native");
    let path = root.join("lib.rock").canonicalize().unwrap();
    let sources = nav_ffi::load_rock(&root).unwrap();
    let file = sources
        .files
        .iter()
        .find(|file| std::path::Path::new(&file.borrow().file.to_id_path()) == path)
        .unwrap();
    let before = "data: Ptr<NativeNavInt>, len: int, cluster: int";
    {
        let mut file = file.borrow_mut();
        assert!(file.contents.contains(before));
        file.contents = file
            .contents
            .replace(before, "data: Ptr<int>, len: int, cluster: int");
    }
    let Err(error) = nav_ffi::generate(&sources, RUST) else {
        panic!("a same-arity pointer mismatch was accepted");
    };
    assert!(
        error.contains("stone_nav_layout_valid: data: Rock has Ptr<int>"),
        "{error}"
    );
}
