use super::common;

const SAXPY: &str = r#"
value SaxpyParams { a: int, n: int }
value GlobalInvocationId { x: int, y: int, z: int }

@shader(compute)
@workgroup(64)
fn saxpy(
    @storage(read) x: StorageBuffer<int>,
    @storage(read) y: StorageBuffer<int>,
    @storage(read_write) out: StorageBuffer<int>,
    @uniform params: SaxpyParams,
) {
    let i = global_invocation_id().x
    if i >= params.n { return }
    storage_store(out, i,
        params.a * storage_load(x, i) + storage_load(y, i))
}

global SAXPY: string = rock_compute(saxpy)
"#;

#[test]
fn valid_saxpy_compute_declaration_is_admitted() {
    let errors = common::bone_rejections(SAXPY);
    assert!(errors.is_empty(), "valid compute declaration was rejected: {errors:#?}");
}

#[test]
fn missing_compute_root_is_reported_by_the_compute_population() {
    let source = SAXPY.replace("global SAXPY: string = rock_compute(saxpy)", "");
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("compute root")),
        "missing root must be diagnosed by the compute collector: {errors:#?}"
    );
}

#[test]
fn an_indirect_compute_root_is_rejected() {
    let source = SAXPY.replace(
        "global SAXPY: string = rock_compute(saxpy)",
        "global ROOT_FN: any = saxpy\nglobal ROOT: string = rock_compute(ROOT_FN)",
    );
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("direct reference")),
        "indirect compute root must reject at its argument: {errors:#?}"
    );
}

#[test]
fn zero_workgroup_size_is_rejected() {
    let source = SAXPY.replace("@workgroup(64)", "@workgroup(0)");
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("workgroup size")),
        "zero workgroup size must be rejected: {errors:#?}"
    );
}

#[test]
fn storage_store_requires_read_write_access() {
    let source = SAXPY.replace("@storage(read_write) out", "@storage(read) out");
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("storage_store") && error.contains("read")),
        "store through a read resource must reject: {errors:#?}"
    );
}

#[test]
fn storage_load_rejects_write_only_access() {
    let source = SAXPY.replace("@storage(read) x", "@storage(write) x");
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("storage_load") && error.contains("write")),
        "load through a write-only resource must reject: {errors:#?}"
    );
}

#[test]
fn graphics_entries_cannot_declare_compute_storage() {
    let source = SAXPY.replace("@shader(compute)", "@shader(fragment)");
    let errors = common::bone_rejections(&source);
    assert!(
        errors.iter().any(|error| error.contains("compute") && error.contains("storage")),
        "graphics storage must reject: {errors:#?}"
    );
}

#[test]
fn global_invocation_id_is_gpu_only() {
    let source = r#"
value GlobalInvocationId { x: int, y: int, z: int }
fn cpu() { let id = global_invocation_id() }
"#;
    let errors = common::bone_rejections(source);
    assert!(
        errors.iter().any(|error| error.contains("GPU-only")),
        "CPU invocation of global_invocation_id must reject: {errors:#?}"
    );
}
