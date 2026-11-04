//! **The interface plan is not a GLSL plan under another name.**
//!
//! `ShaderInterfacePlan` claims to be target-neutral and `ShaderAbiPlan` claims
//! to be GLSL's answer to it. Two objects with those names prove nothing on
//! their own -- if the interface plan already carried GLSL's bindings, units
//! and offsets, splitting it in two would be a rename.
//!
//! So a HOSTILE second target derives its own ABI from the SAME
//! `ShaderInterfacePlan` and answers deliberately differently: bindings in
//! reverse role order, texture units in reverse declaration order, and members
//! packed tightly instead of by std140. It is synthetic on purpose -- the point
//! is not that anyone wants this layout, it is that the interface plan is
//! capable of producing it, which it could not be if the physical answer lived
//! upstream.
//!
//! The DENOMINATOR is asserted in every case: a fixture with no uniform group,
//! no samplers and no attributes would make "the two targets disagree" vacuous,
//! so each arm states the population it disagreed over.

use skull_codegen_glsl::{GlslProfile, ShaderAbiPlan};
use skull_mir::gpu_layout::WireKind;
use skull_mir::shader_ir::{ShaderInterfacePlan, ShaderStage as IrStage, UniformRole};
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

/// A pipeline with all four interface populations: per-vertex and per-instance
/// attributes, a uniform group per stage plus an engine group, and a texture
/// group with two samplers.
const PIPELINE: &str = r#"
value QuadVert { pos: Vec2, texcoord: Vec2 }
value Inst { shift: Vec2, inst_tint: Vec4 }
value VertU { scale: float }
value FragU { cutoff: float, tint: Vec4 }
value EngineU { time: float, resolution: Vec2 }
struct Tex { atlas: Sampler2D, noise: Sampler2D }
value Vary { position: Vec4, uv: Vec2 }

@shader(vertex)
fn hv(@attr a: QuadVert, @instance i: Inst, @uniform u: VertU, @engine e: EngineU) -> Vary {
    return Vary { position: v(a.pos.x * u.scale + i.shift.x, a.pos.y, 0.0, 1.0), uv: a.texcoord }
}

@shader(fragment)
fn hf(vary: Vary, @uniform u: FragU, @engine e: EngineU, @textures t: Tex) -> Vec4 {
    let c = texture(t.atlas, vary.uv)
    let n = texture(t.noise, vary.uv)
    return v(c.r * u.cutoff, n.g, c.b, 1.0)
}

fn main() { let k = rock_shader(hv, hf) }
"#;

/// A second target's ABI: every physical decision made the OTHER way.
///
/// Not a plausible layout -- a deliberately contrary one, so a shared answer
/// leaking through from the interface plan cannot be mistaken for agreement.
struct HostileAbi {
    /// Role -> binding, assigned in REVERSE role order.
    bindings: Vec<(UniformRole, u32)>,
    /// Sampler name -> unit, assigned in REVERSE declaration order.
    units: Vec<(Ustr, u32)>,
    /// Member -> offset, packed TIGHTLY (no std140 alignment or 16-byte round
    /// up), which is what makes an offset disagreement possible at all.
    offsets: Vec<(Ustr, u32)>,
}

fn tight_size(k: WireKind) -> u32 {
    match k {
        WireKind::F32 | WireKind::I32 | WireKind::U32 | WireKind::Bool32 => 4,
        WireKind::F32x2 => 8,
        WireKind::F32x3 => 12,
        WireKind::F32x4 => 16,
        WireKind::Mat4 => 64,
    }
}

impl HostileAbi {
    /// Derived from the interface plan ALONE -- the same input `ShaderAbiPlan::glsl`
    /// gets, and nothing else.
    fn of(iface: &ShaderInterfacePlan) -> Self {
        let n = iface.uniform_groups.len() as u32;
        let bindings = iface
            .uniform_groups
            .iter()
            .enumerate()
            .map(|(i, g)| (g.role, n - 1 - i as u32))
            .collect();
        let names = iface.sampler_names();
        let m = names.len() as u32;
        let units = names
            .iter()
            .enumerate()
            .map(|(i, s)| (*s, m - 1 - i as u32))
            .collect();
        let mut offsets = Vec::new();
        for g in &iface.uniform_groups {
            let mut at = 0u32;
            for (name, kind) in &g.members {
                offsets.push((*name, at));
                at += tight_size(*kind);
            }
        }
        HostileAbi {
            bindings,
            units,
            offsets,
        }
    }
}

#[test]
fn a_second_target_derives_a_different_physical_layout_from_the_same_interface() {
    let (ws, mid) = workspace(PIPELINE);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        "hv",
        "hf",
        GlslProfile::Core330,
    )
    .expect("the fixture must emit");
    let iface = &em.interface;
    let glsl = &em.abi;
    let hostile = HostileAbi::of(iface);

    // ---- DENOMINATORS. Each disagreement below is measured over a stated,
    // non-empty population; without these, "they differ" could be "both empty".
    assert_eq!(
        iface.uniform_groups.len(),
        3,
        "engine + vertex + fragment groups"
    );
    assert_eq!(iface.sampler_names().len(), 2, "two samplers");
    assert_eq!(
        iface.vertex_inputs.len(),
        2,
        "per-vertex and per-instance inputs"
    );
    assert!(
        iface.varyings.iter().any(|v| v.name.as_str() == "uv"),
        "{:?}",
        iface.varyings
    );
    assert!(!hostile.offsets.is_empty());

    // ---- BINDINGS disagree. GLSL puts @engine at 0 by role; the hostile
    // target puts it last.
    let glsl_engine = glsl
        .uniform_blocks
        .iter()
        .find(|(_, p)| p.plan.struct_name.as_str() == "EngineU")
        .map(|(b, _)| *b)
        .expect("the engine block");
    let hostile_engine = hostile
        .bindings
        .iter()
        .find(|(r, _)| *r == UniformRole::Engine)
        .map(|(_, b)| *b)
        .expect("the engine role");
    assert_eq!(glsl_engine, 0, "GLSL reserves binding 0 for @engine");
    assert_ne!(
        glsl_engine, hostile_engine,
        "both targets put @engine at the same binding, so the interface plan decided it"
    );

    // ---- UNITS disagree, over a population of two.
    let glsl_units: Vec<(Ustr, u32)> = glsl
        .texture_units
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, i as u32))
        .collect();
    assert_ne!(
        glsl_units, hostile.units,
        "both targets assigned the same sampler units, so the interface plan decided them"
    );
    // ...but they agree on WHICH samplers exist and in what declared order --
    // that IS the interface's answer, and the disagreement above is only
    // meaningful because this holds.
    let mut a: Vec<Ustr> = glsl_units.iter().map(|(n, _)| *n).collect();
    let mut b: Vec<Ustr> = hostile.units.iter().map(|(n, _)| *n).collect();
    a.sort();
    b.sort();
    assert_eq!(
        a, b,
        "the two targets disagree about which samplers exist, not just where"
    );

    // ---- OFFSETS disagree: std140 aligns `resolution: Vec2` to 8 and rounds
    // the block to 16; the hostile target packs tightly.
    let glsl_offsets: Vec<(Ustr, u32)> = glsl
        .uniform_blocks
        .iter()
        .flat_map(|(_, p)| p.plan.members.iter().map(|m| (m.name, m.offset)))
        .collect();
    assert_ne!(
        glsl_offsets, hostile.offsets,
        "both targets packed identically, so std140 is not GLSL's own answer"
    );

    // ---- The IR is the program's real bodies, per stage, and its declaration
    // set is what a reload watches. A second backend renders THESE.
    assert_eq!(
        em.ir.stages.len(),
        2,
        "a pipeline publishes two stages of IR"
    );
    let names: Vec<&str> = em
        .ir
        .stages
        .iter()
        .map(|s| {
            ws.module(s.entry.fref().module).functions[s.entry.fref().func]
                .name
                .as_str()
        })
        .collect();
    assert_eq!(
        names,
        vec!["hv", "hf"],
        "the IR's stages are not the program's entries"
    );
    // The stages carry their own kind, and the vertex stage's body really is
    // the vertex entry's -- not a clone of the fragment stage's.
    assert_eq!(
        em.ir.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
        vec![IrStage::Vertex, IrStage::Fragment]
    );
    assert_ne!(
        em.ir.stages[0].entry.fref(),
        em.ir.stages[1].entry.fref(),
        "both stages published the same entry instance"
    );
    assert_ne!(
        em.ir.stages[0].body.blocks.len() + em.ir.stages[0].body.insts.len(),
        0,
        "the vertex stage published an empty body"
    );
    // The reload set is exactly the program's distinct declarations -- the two
    // entries here, deduped across stages rather than concatenated.
    let decls = em.ir.declarations();
    let mut unique = decls.clone();
    unique.sort_by_key(|f| (f.module.raw(), f.func.into_raw().into_u32()));
    unique.dedup();
    assert_eq!(
        decls.len(),
        unique.len(),
        "the reload set repeats a declaration: {decls:?}"
    );
    assert!(
        decls.len() >= 2,
        "the IR reports no declarations to reload: {decls:?}"
    );

    // ---- The INTERFACE itself names nothing physical. A round trip through a
    // second derivation cannot change it, because it was never asked.
    let regenerated = ShaderAbiPlan::glsl(iface, Default::default())
        .expect("the interface plan is sufficient to rebuild the GLSL ABI");
    assert_eq!(
        regenerated.texture_units, glsl.texture_units,
        "the ABI is not reproducible from the interface alone"
    );
    assert_eq!(
        regenerated
            .uniform_blocks
            .iter()
            .map(|(b, _)| *b)
            .collect::<Vec<_>>(),
        glsl.uniform_blocks
            .iter()
            .map(|(b, _)| *b)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        regenerated.attribute_locations, glsl.attribute_locations,
        "attribute locations are not reproducible from the interface alone"
    );
}

/// **The interface plan carries no GLSL vocabulary.** A guard on the claim
/// above: its `Debug` rendering must not mention a GLSL type spelling, a
/// binding, an offset or `std140`. Cheap, and it fails the moment someone adds
/// a convenience field that leaks the target's answer upward.
///
/// The check is TEXTUAL, so the fixture deliberately uses no field named after
/// a banned word -- an `Inst.offset` field made it red while nothing was wrong.
/// What makes it worth keeping anyway is that `Vec2`/`Vec4`/`Sampler2D` DO
/// appear in the fixture's source and must NOT appear in the rendering: they
/// arrive as `F32x2`/`F32x4`/`Float`, which is the property under test.
#[test]
fn the_interface_plan_names_nothing_physical() {
    let (ws, mid) = workspace(PIPELINE);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        "hv",
        "hf",
        GlslProfile::Core330,
    )
    .expect("the fixture must emit");
    let rendered = format!("{:?}", em.interface);

    // The denominator: the fixture's interface is populated, so the absences
    // below are absences and not an empty string.
    assert!(
        rendered.contains("EngineU") && rendered.contains("atlas"),
        "{rendered}"
    );

    for banned in [
        "vec4",
        "vec2",
        "sampler2D",
        "std140",
        "binding",
        "offset",
        "layout",
    ] {
        assert!(
            !rendered.to_lowercase().contains(banned),
            "the interface plan names `{banned}`, which is a target's answer:\n{rendered}"
        );
    }
}

/// **The ABI lays out the INTERFACE's members, not the struct they came from.**
///
/// The strongest available statement that the split is an ownership boundary
/// and not a projection: perturb a member's kind in the interface plan alone --
/// touching no source, no `Workspace`, no struct -- and the derived std140
/// layout must move. If `ShaderAbiPlan::glsl` re-read the declaring struct, the
/// perturbed plan would produce the ORIGINAL layout and this is the only test
/// that can see it.
#[test]
fn perturbing_an_interface_member_moves_the_derived_layout() {
    let (ws, mid) = workspace(PIPELINE);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        "hv",
        "hf",
        GlslProfile::Core330,
    )
    .expect("the fixture must emit");
    // Dropped before the layout below is derived: `ShaderAbiPlan::glsl` does
    // not take a `Workspace` at all, so this is a statement about the signature
    // rather than about discipline.
    drop(ws);

    let engine_of = |p: &skull_codegen_glsl::ShaderAbiPlan| {
        p.uniform_blocks
            .iter()
            .find(|(_, b)| b.plan.struct_name.as_str() == "EngineU")
            .map(|(_, b)| {
                (
                    b.plan.size,
                    b.plan
                        .members
                        .iter()
                        .map(|m| (m.name, m.offset))
                        .collect::<Vec<_>>(),
                )
            })
            .expect("the engine block")
    };
    let before = engine_of(&em.abi);

    // The DENOMINATOR: `EngineU` is `{ time: float, resolution: Vec2 }`, so
    // std140 puts `resolution` at 8 (align 8 after a 4-byte float). Widening
    // `time` to a Vec4 must push it, and a re-read of the struct cannot.
    assert_eq!(
        before.1.len(),
        2,
        "the engine group lost a member: {before:?}"
    );
    assert_eq!(before.1[0], (Ustr::from("time"), 0));
    assert_eq!(before.1[1], (Ustr::from("resolution"), 8));

    let mut perturbed = em.interface.clone();
    let g = perturbed
        .uniform_groups
        .iter_mut()
        .find(|g| g.struct_name.key.as_str() == "EngineU")
        .expect("the engine group");
    assert_eq!(
        g.members[0].1,
        WireKind::F32,
        "the fixture's first member is not the float"
    );
    g.members[0].1 = WireKind::F32x4;

    let after = engine_of(
        &ShaderAbiPlan::glsl(&perturbed, Default::default())
            .expect("the perturbed interface must lay out"),
    );
    assert_ne!(
        before, after,
        "the derived layout ignored the perturbed member, so it was read from the struct"
    );
    assert_eq!(
        after.1[1],
        (Ustr::from("resolution"), 16),
        "the widened member did not push its neighbour: {after:?}"
    );
}

/// **A BARE sampler is a logical resource too, and it is not a compiler slot.**
///
/// A loose `Sampler2D` param keeps a CPU-assigned unit, while a `@textures`
/// group member gets a compiler-assigned one -- so the interface must carry
/// both and keep them apart, or the engine cannot tell which units it owns.
/// Mixing the two in ONE program is refused (`logical_textures`), which is why
/// the bare case needs its own fixture rather than an extra param above.
#[test]
fn a_bare_sampler_reaches_the_interface_and_takes_no_compiler_unit() {
    let src = r#"
@shader(fragment)
fn bare_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    return texture(uTexture, tc)
}
fn main() { let k = rock_shader(bare_frag) }
"#;
    let (ws, mid) = workspace(src);
    let em = skull_codegen_glsl::emit_shader_entry_with_dependencies(
        &ws,
        mid,
        "bare_frag",
        GlslProfile::Core330,
    )
    .expect("a bare sampler entry must emit");

    // It IS in the interface, with its kind.
    assert_eq!(
        em.interface
            .bare_samplers
            .iter()
            .map(|s| (s.name, s.kind, s.stage))
            .collect::<Vec<_>>(),
        vec![(
            Ustr::from("uTexture"),
            skull_mir::gpu_layout::SamplerKind::Float,
            IrStage::Fragment
        )],
        "a bare sampler is missing from the interface"
    );
    // ...and it takes NO compiler-assigned unit: the group is what those come
    // from, and there is no group here.
    assert!(
        em.interface.textures.is_none(),
        "a bare sampler invented a @textures group"
    );
    assert!(
        em.abi.texture_units.is_empty(),
        "a bare sampler was given a compiler slot: {:?}",
        em.abi.texture_units
    );
    // The declaration still reaches GLSL, from the interface.
    assert!(
        em.source.contains("uniform sampler2D uTexture;"),
        "{}",
        em.source
    );

    // CONTROL: a GROUPED sampler does take a unit, so the emptiness above is
    // about bareness and not about samplers being absent from the ABI.
    let grouped = r#"
struct Tex { atlas: Sampler2D }
@shader(fragment)
fn grouped_frag(@textures t: Tex, tc: Vec2) -> Vec4 {
    return texture(t.atlas, tc)
}
fn main() { let k = rock_shader(grouped_frag) }
"#;
    let (ws2, mid2) = workspace(grouped);
    let em2 = skull_codegen_glsl::emit_shader_entry_with_dependencies(
        &ws2,
        mid2,
        "grouped_frag",
        GlslProfile::Core330,
    )
    .expect("a grouped sampler entry must emit");
    assert_eq!(em2.abi.texture_units, vec![Ustr::from("atlas")]);
    assert!(em2.interface.bare_samplers.is_empty());
}

/// **Attribute LOCATIONS are the target's answer.** The hostile target numbers
/// them backwards from the same interface, and GLSL's emitted
/// `layout (location = N)` must come from the ABI plan rather than from a
/// second walk -- otherwise the two could disagree and only the driver notices.
#[test]
fn a_second_target_numbers_attribute_locations_differently() {
    let (ws, mid) = workspace(PIPELINE);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        "hv",
        "hf",
        GlslProfile::Core330,
    )
    .expect("the fixture must emit");

    // DENOMINATOR: four attributes across two input structs.
    let n = em
        .interface
        .vertex_inputs
        .iter()
        .map(|i| i.attrs.len())
        .sum::<usize>();
    assert_eq!(
        n, 4,
        "the fixture lost attributes: {:?}",
        em.interface.vertex_inputs
    );
    assert_eq!(em.abi.attribute_locations.len(), n);

    // GLSL numbers them in declaration order, continuing across structs.
    assert_eq!(
        em.abi.attribute_locations,
        vec![
            (Ustr::from("pos"), 0),
            (Ustr::from("texcoord"), 1),
            (Ustr::from("shift"), 2),
            (Ustr::from("inst_tint"), 3),
        ]
    );
    // ...and each COMPLETE declaration line is emitted: name, type and location
    // together. Checking that each location occurs somewhere and each name
    // occurs somewhere survives a permuted mapping, which is exactly the drift
    // this is meant to catch.
    let expect = |name: &str, ty: &str, loc: u32| {
        let line = format!("layout (location = {loc}) in {ty} {name};");
        assert!(
            em.source.contains(&line),
            "missing declaration `{line}`:\n{}",
            em.source
        );
    };
    expect("pos", "vec2", 0);
    expect("texcoord", "vec2", 1);
    expect("shift", "vec2", 2);
    expect("inst_tint", "vec4", 3);
    // The pairing is what is asserted, so a SWAPPED pair must not be present.
    for bad in [
        "layout (location = 0) in vec2 texcoord;",
        "layout (location = 1) in vec2 pos;",
        "layout (location = 3) in vec2 shift;",
    ] {
        assert!(
            !em.source.contains(bad),
            "a permuted declaration `{bad}` is present"
        );
    }

    // The hostile target numbers them backwards from the SAME interface.
    let hostile: Vec<(Ustr, u32)> = {
        let mut flat: Vec<Ustr> = em
            .interface
            .vertex_inputs
            .iter()
            .flat_map(|i| i.attrs.iter().map(|(n, _)| *n))
            .collect();
        flat.reverse();
        flat.iter()
            .enumerate()
            .map(|(i, n)| (*n, i as u32))
            .collect()
    };
    assert_ne!(
        em.abi.attribute_locations, hostile,
        "both targets numbered attributes alike, so the interface plan decided it"
    );
}

/// **A sampler is declared in every stage that USES it.**
///
/// `@textures` is one group per PROGRAM, but the GLSL declaration is per
/// STAGE: each stage's `#ifdef` block is its own translation unit, so a stage
/// that samples needs the `uniform sampler2D` in its own section. Vertex
/// texture fetch is legal in GL 3.3 / GLES 3.0, and the vertex stage was
/// emitting a reference to an identifier it never declared.
///
/// The fixture has BOTH stages sampling the same group, so the assertion is
/// about per-stage declaration rather than about the group existing.
#[test]
fn a_sampler_is_declared_in_each_stage_that_uses_it() {
    let src = r#"
value QuadVert { pos: Vec2, texcoord: Vec2 }
struct Tex { height: Sampler2D }
value Vary { position: Vec4, uv: Vec2 }

@shader(vertex)
fn tv(@attr a: QuadVert, @textures t: Tex) -> Vary {
    let h = texture(t.height, a.texcoord)
    return Vary { position: v(a.pos.x, a.pos.y + h.r, 0.0, 1.0), uv: a.texcoord }
}

@shader(fragment)
fn tf(vary: Vary, @textures t: Tex) -> Vec4 {
    return texture(t.height, vary.uv)
}

fn main() { let k = rock_shader(tv, tf) }
"#;
    let (ws, mid) = workspace(src);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        "tv",
        "tf",
        GlslProfile::Core330,
    )
    .expect("a pipeline sampling in both stages must emit");

    // The DENOMINATOR: one group, one sampler, used by two stages.
    assert_eq!(em.abi.texture_units, vec![Ustr::from("height")]);

    // Each stage's own section must DECLARE it, not just reference it.
    let (vert, frag) = {
        let v = em.source.find("#ifdef VERTEX").expect("a vertex section");
        let p = em.source.find("#ifdef PIXEL").expect("a pixel section");
        if v < p {
            (&em.source[v..p], &em.source[p..])
        } else {
            (&em.source[v..], &em.source[p..v])
        }
    };
    for (label, section) in [("vertex", vert), ("fragment", frag)] {
        assert!(
            section.contains("uniform sampler2D height;"),
            "the {label} stage references `height` without declaring it:\n{section}"
        );
    }
}
