use crate::common::lower;
use skull_mir::mir::{ContainerKind, Inst, Lit, MirFunc};

fn fields(body: &MirFunc) -> usize {
    body.insts
        .iter()
        .filter(|(_, inst)| {
            matches!(inst, Inst::Field { .. })
                || matches!(
                    inst,
                    Inst::Index {
                        index,
                        kind: ContainerKind::Record,
                        ..
                    } if matches!(&body.insts[*index], Inst::Const(Lit::Str(_)))
                )
        })
        .count()
}

fn wide_fields() -> String {
    (0..50)
        .map(|i| format!("f{i}: int"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn nullable_native_field_snapshot_reads_scalar_aliases() {
    let source = "value Point { x: int, y: int }\n\
        value Span { a: Point, b: Point }\n\
        value State { active: bool, span: Span? }\n\
        struct Owner { state: State }\n\
        fn ready(state: State) -> bool { state.active }\n\
        fn snapshot(owner: Owner) -> bool { let state = owner.state; ready(state) }";
    let body = lower(source, "snapshot");
    let child_fields = body
        .insts
        .iter()
        .filter(|(_, inst)| {
            matches!(inst, Inst::Field { field, .. } if matches!(field.as_str(), "a" | "b"))
        })
        .count();
    assert_eq!(
        child_fields, 0,
        "snapshot must not create child cdata views: {body}"
    );
    let scalar_aliases = body
        .insts
        .iter()
        .filter(|(_, inst)| {
            matches!(inst, Inst::Field { field, .. } if field.as_str().starts_with("__rock_leaf_"))
        })
        .count();
    assert_eq!(
        scalar_aliases, 4,
        "snapshot reads every scalar leaf: {body}"
    );
}

#[test]
fn nullable_scalar_projection_uses_the_embedded_owner_slot() {
    let source = "value Label { text: string, width: int }\n\
        struct Inner { label: Label?, tag: int }\n\
        struct Root { embed inner: Inner }\n\
        fn width(r: Root) -> int? { r.inner.label?.width }";
    let body = lower(source, "width");
    assert_eq!(fields(&body), 2, "embedded owner projection: {body}");
}

#[test]
fn boxed_nullable_subtree_reads_its_presence_and_payload_only() {
    let source = format!(
        "nobox value Point {{ x: int, y: int }}\n\
         value Wide {{ {}, child: Point? }}\n\
         fn pick(v: Wide) -> Point? {{ v.child }}",
        wide_fields()
    );
    let body = lower(&source, "pick");
    assert_eq!(fields(&body), 3, "nullable subtree keeps presence: {body}");
}

#[test]
fn leaf_table_reference_projects_before_expanding_siblings() {
    let source = "nobox value Point { x: int, y: int }\n\
        value State { armed: bool, point: Point, anchor: Point?, age: float }\n\
        fn flag(p: ref State) -> bool { p.armed }\n\
        fn point(p: ref State) -> Point { p.point }\n\
        fn anchor(p: ref State) -> Point? { p.anchor }";
    let flag = lower(source, "flag");
    assert_eq!(fields(&flag), 1, "scalar ref projection: {flag}");
    let point = lower(source, "point");
    assert_eq!(fields(&point), 2, "selected subtree snapshot: {point}");
    let anchor = lower(source, "anchor");
    assert_eq!(
        fields(&anchor),
        3,
        "nullable subtree keeps presence: {anchor}"
    );
}
