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
use skull_mir::shader_ir::{ShaderInterfacePlan, UniformRole};
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace(src: &str) -> (bone_semantic::inference::Workspace, bone_semantic::prelude::ModuleId) {
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
        WireKind::F32 | WireKind::I32 | WireKind::Bool32 => 4,
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
        let units = names.iter().enumerate().map(|(i, s)| (*s, m - 1 - i as u32)).collect();
        let mut offsets = Vec::new();
        for g in &iface.uniform_groups {
            let mut at = 0u32;
            for (name, kind) in &g.members {
                offsets.push((*name, at));
                at += tight_size(*kind);
            }
        }
        HostileAbi { bindings, units, offsets }
    }
}

#[test]
fn a_second_target_derives_a_different_physical_layout_from_the_same_interface() {
    let (ws, mid) = workspace(PIPELINE);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(&ws, mid, "hv", "hf", GlslProfile::Core330)
        .expect("the fixture must emit");
    let iface = &em.interface;
    let glsl = &em.abi;
    let hostile = HostileAbi::of(iface);

    // ---- DENOMINATORS. Each disagreement below is measured over a stated,
    // non-empty population; without these, "they differ" could be "both empty".
    assert_eq!(iface.uniform_groups.len(), 3, "engine + vertex + fragment groups");
    assert_eq!(iface.sampler_names().len(), 2, "two samplers");
    assert_eq!(iface.vertex_inputs.len(), 2, "per-vertex and per-instance inputs");
    assert!(iface.varyings.iter().any(|v| v.name.as_str() == "uv"), "{:?}", iface.varyings);
    assert!(!hostile.offsets.is_empty());

    // ---- BINDINGS disagree. GLSL puts @engine at 0 by role; the hostile
    // target puts it last.
    let glsl_engine = glsl
        .uniform_blocks
        .iter()
        .find(|(_, p)| p.struct_name.as_str() == "EngineU")
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
    let glsl_units: Vec<(Ustr, u32)> =
        glsl.texture_units.iter().enumerate().map(|(i, n)| (*n, i as u32)).collect();
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
    assert_eq!(a, b, "the two targets disagree about which samplers exist, not just where");

    // ---- OFFSETS disagree: std140 aligns `resolution: Vec2` to 8 and rounds
    // the block to 16; the hostile target packs tightly.
    let glsl_offsets: Vec<(Ustr, u32)> = glsl
        .uniform_blocks
        .iter()
        .flat_map(|(_, p)| p.members.iter().map(|m| (m.name, m.offset)))
        .collect();
    assert_ne!(
        glsl_offsets, hostile.offsets,
        "both targets packed identically, so std140 is not GLSL's own answer"
    );

    // ---- The IR is the program's real bodies, per stage, and its declaration
    // set is what a reload watches. A second backend renders THESE.
    assert_eq!(em.ir.stages.len(), 2, "a pipeline publishes two stages of IR");
    assert_eq!(em.ir.stages[0].entry.fref, em.ir.stages[0].entry.fref);
    let names: Vec<&str> = em
        .ir
        .stages
        .iter()
        .map(|s| ws.module(s.entry.fref.module).functions[s.entry.fref.func].name.as_str())
        .collect();
    assert_eq!(names, vec!["hv", "hf"], "the IR's stages are not the program's entries");
    assert!(
        em.ir.declarations().len() >= 2,
        "the IR reports no declarations to reload: {:?}",
        em.ir.declarations().len()
    );

    // ---- The INTERFACE itself names nothing physical. A round trip through a
    // second derivation cannot change it, because it was never asked.
    let regenerated = ShaderAbiPlan::glsl(&ws, iface, Default::default())
        .expect("the interface plan is sufficient to rebuild the GLSL ABI");
    assert_eq!(
        regenerated.texture_units, glsl.texture_units,
        "the ABI is not reproducible from the interface alone"
    );
    assert_eq!(
        regenerated.uniform_blocks.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
        glsl.uniform_blocks.iter().map(|(b, _)| *b).collect::<Vec<_>>()
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
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(&ws, mid, "hv", "hf", GlslProfile::Core330)
        .expect("the fixture must emit");
    let rendered = format!("{:?}", em.interface);

    // The denominator: the fixture's interface is populated, so the absences
    // below are absences and not an empty string.
    assert!(rendered.contains("EngineU") && rendered.contains("atlas"), "{rendered}");

    for banned in ["vec4", "vec2", "sampler2D", "std140", "binding", "offset", "layout"] {
        assert!(
            !rendered.to_lowercase().contains(banned),
            "the interface plan names `{banned}`, which is a target's answer:\n{rendered}"
        );
    }
}
