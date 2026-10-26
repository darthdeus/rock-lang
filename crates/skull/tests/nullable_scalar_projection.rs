use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
value ProjectionPoint { x: int, y: int }
value ProjectionLabel { text: string, point: ProjectionPoint, width: int, enabled: bool, maybe: int? }
struct ProjectionOwner { label: ProjectionLabel? = null, point: ProjectionPoint? = null }
struct ProjectionCounter { calls: int = 0 }
@nomangle fn next_owner(h: ProjectionOwner, c: ProjectionCounter) -> ProjectionOwner {
    c.calls += 1
    return h
}
@nomangle fn selected_width(h: ProjectionOwner, c: ProjectionCounter) -> int {
    return next_owner(h, c).label?.width ?? -1
}
@nomangle fn selected_x(h: ProjectionOwner) -> int { return h.point?.x ?? -1 }
fn mutate_label(h: ProjectionOwner) -> int {
    if h.label != null { h.label.width = 99; h.label.point.x = 101 }
    return 0
}
fn first(a: int, b: int) -> int { return a }
fn main() {
    let h = ProjectionOwner {}
    let c = ProjectionCounter {}
    print("{selected_width(h, c)} {selected_x(h)} {c.calls}")
    h.label = ProjectionLabel {
        text: "old", point: ProjectionPoint { x: 7, y: 9 },
        width: 11, enabled: false, maybe: null
    }
    h.point = ProjectionPoint { x: 13, y: 17 }
    print("{selected_width(h, c)} {selected_x(h)} {c.calls}")
    print("{h.label?.text ?? "missing"} {h.label?.enabled ?? true} {h.label?.maybe ?? -1}")
    let whole = h.label
    let point = h.label?.point
    let before = first(h.label?.width ?? -1, mutate_label(h))
    print("{before} {h.label?.width ?? -1} {whole?.width ?? -1} {point?.x ?? -1}")
    if whole != null { print("{whole.point.x}") }
}
"#;

#[test]
fn nullable_scalar_projection_preserves_effects_absence_and_snapshots() {
    let lua = emit_module(SOURCE);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nullable_scalar_projection"),
        "-1 -1 1\n11 13 2\nold false -1\n11 99 11 7\n7");
}

#[test]
fn nullable_scalar_projection_emits_one_receiver_and_no_sibling_reads() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "selected_width");
    assert_eq!(body.matches("next_owner(").count(), 1, "{body}");
    for field in [".text", ".point", ".enabled", ".maybe"] {
        assert!(!body.contains(field), "unused sibling {field}: {body}");
    }
    let point = function_body(&lua, "selected_x");
    assert!(!point.contains(".y"), "{point}");
}

#[test]
fn nullable_scalar_projection_reads_an_embedded_owner_without_materializing_it() {
    let source = r#"
value EmbeddedLabel { text: string, width: int }
struct EmbeddedOwner { label: EmbeddedLabel? = null, tag: int = 5 }
struct EmbeddedRoot { embed inner: EmbeddedOwner }
@nomangle fn embedded_width(r: EmbeddedRoot) -> int { r.inner.label?.width ?? -1 }
fn main() {
    let r = EmbeddedRoot { inner: EmbeddedOwner {} }
    print("{embedded_width(r)}")
    r.inner.label = EmbeddedLabel { text: "row", width: 23 }
    print("{embedded_width(r)} {r.inner.tag}")
}
"#;
    let lua = emit_module(source);
    let body = function_body(&lua, "embedded_width");
    assert!(!body.contains("inner.tag"), "{body}");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nullable_embedded_projection"),
        "-1\n23 5");
}
