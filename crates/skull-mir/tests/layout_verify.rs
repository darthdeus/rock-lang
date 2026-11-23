use bone_core::hir::FnBackend;
use skull_mir::mir::{Inst, Lit, MirFunc};
use skull_mir::verify::{Stage, Violation};
use ustr::Ustr;

fn layout_violations(source: &str, name: &str, corrupt: impl FnOnce(&mut MirFunc)) -> Vec<String> {
    let ast = skull_parser::parse_module_clean(source, Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic pass");
    let mut body =
        crate::common::MirFixture::new(&ws, &[crate::common::decl(&ws, name)]).named(name);
    let clean = skull_mir::verify::verify_with_layouts(&ws, FnBackend::Lua, &body, Stage::LOWERED);
    assert!(
        clean.is_empty(),
        "lowered body must verify: {clean:?}\n{body}"
    );
    corrupt(&mut body);
    skull_mir::verify::verify_with_layouts(&ws, FnBackend::Lua, &body, Stage::LOWERED)
        .into_iter()
        .map(|Violation { message, .. }| message)
        .collect()
}

fn rename_field(body: &mut MirFunc, from: &str, to: &str) {
    let renamed = body.insts.iter_mut().find_map(|(_, inst)| match inst {
        Inst::Field { field, .. }
        | Inst::StoreField { field, .. }
        | Inst::Const(Lit::Str(field))
            if field.as_str() == from =>
        {
            *field = Ustr::from(to);
            Some(())
        }
        _ => None,
    });
    assert!(renamed.is_some(), "no access to `{from}`: {body}");
}

const RECORD: &str = "nobox value Point { x: int, y: int }\n\
    struct Body { pos: Point?, mass: float }\n\
    fn mass(b: Body) -> float { b.mass }\n\
    fn shift(b: Body) { b.pos?.x = 3 }";

#[test]
fn a_record_read_must_name_a_stored_key() {
    let messages = layout_violations(RECORD, "mass", |body| {
        rename_field(body, "mass", "pos.mass")
    });
    assert_eq!(
        messages,
        ["reads `pos.mass` of a `Body` record, which does not store it"]
    );
}

#[test]
fn a_presence_tag_has_no_fields() {
    // The leaf `pos.x` written as a field of the presence tag `pos`.
    let messages = layout_violations(RECORD, "shift", |body| {
        let tag = body
            .insts
            .iter()
            .find_map(|(id, inst)| matches!(inst, Inst::Index { .. }).then_some(id))
            .unwrap_or_else(|| panic!("presence read: {body}"));
        let (_, store) = body
            .insts
            .iter_mut()
            .find(|(_, inst)| matches!(inst, Inst::StoreIndex { .. }))
            .unwrap_or_else(|| panic!("guarded leaf store"));
        let Inst::StoreIndex { value, .. } = *store else {
            unreachable!()
        };
        *store = Inst::StoreField {
            base: tag,
            field: Ustr::from("x"),
            value,
            native_leaf: None,
        };
    });
    assert_eq!(
        messages,
        ["writes `x` of a presence tag, which has no fields"]
    );
}

#[test]
fn a_value_constructor_read_must_name_a_written_key() {
    let source = "nobox value Leaf { a: int, b: string }\n\
        value Parent { k: int, l: Leaf, z: int }\n\
        fn read(k: int) -> int { let p = Parent { k: k, l: Leaf { a: 2, b: \"x\" }, z: 3 }\n p.l.a }";
    let messages = layout_violations(source, "read", |body| rename_field(body, "l.a", "l"));
    assert_eq!(
        messages,
        ["reads `l` of a `Parent` table, which does not store it"]
    );
}

#[test]
fn a_pass_may_not_move_a_store_into_a_dropped_temporary() {
    let source = "value V2 { x: float, y: float }\n\
        value Holder { name: string, p: V2 }\n\
        fn bump(x: float) -> float {\n\
            let h = Holder { name: \"h\", p: V2 { x: x, y: 0.0 } }\n\
            h.p.x = 5.0\n\
            h.p.x\n\
        }";
    let ast = skull_parser::parse_module_clean(source, Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).expect("semantic pass");
    let mut body =
        crate::common::MirFixture::new(&ws, &[crate::common::decl(&ws, "bump")]).named("bump");
    let before = skull_mir::verify::read_stores(&body);
    let lost = |body: &MirFunc| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            skull_mir::verify::assert_no_lost_writes(body, &before, &"the test pass")
        }))
        .err()
        .and_then(|panic| panic.downcast_ref::<String>().cloned())
    };
    assert_eq!(lost(&body), None, "{body}");

    // What a broken scalar replacement does: rebuild the local at the store
    // from the constructor's operands, which the later read still uses.
    let (cell, ctor) = body
        .insts
        .iter()
        .find_map(|(_, inst)| match *inst {
            Inst::Store { cell, value }
                if matches!(body.insts[value], Inst::MakeValueStruct { .. }) =>
            {
                Some((cell, value))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("stored constructor: {body}"));
    let (block, load) = body
        .blocks
        .iter()
        .find_map(|(bid, block)| {
            block
                .insts
                .iter()
                .copied()
                .find(|&id| body.insts[id] == Inst::Load(cell))
                .map(|id| (bid, id))
        })
        .unwrap_or_else(|| panic!("load of the local: {body}"));
    let rebuilt = body.alloc_inst(body.insts[ctor].clone());
    let at = body.blocks[block]
        .insts
        .iter()
        .position(|&id| id == load)
        .expect("listed load");
    body.blocks[block].insts.insert(at, rebuilt);
    let projection = body
        .insts
        .iter_mut()
        .find_map(|(_, inst)| match inst {
            Inst::Field { base, .. } if *base == load => Some(base),
            _ => None,
        })
        .expect("projection of the first load");
    *projection = rebuilt;
    let message = lost(&body).unwrap_or_else(|| panic!("the moved store is reported: {body}"));
    assert!(
        message
            .contains("the test pass moved a field store into a fresh `Holder` that nothing reads"),
        "{message}"
    );
}
