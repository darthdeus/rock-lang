use super::*;
use ustr::Ustr;

fn build_ws(src: &str) -> (Workspace, ModuleId) {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
    (ws, mid)
}

fn lower_test_instance(ws: &Workspace, fref: FuncRef) -> (ScalarCtx, MirFunc) {
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(ws, vec![fref], false, Arc::clone(&interner));
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new_values_first_class(ws).sharing(Arc::clone(&interner));
    let plan = concrete
        .plans
        .get(&Instance::base(fref))
        .expect("shader test concrete plan");
    let mf = skull_mir::lower::lower_instance_with_plans(ws, &scx, plan, &concrete.plans)
        .expect("shader test lower");
    (scx, mf)
}

#[test]
fn concrete_signature_tier0_check_rejects_a_non_tier0_plan_type() {
    let interner = MonoInterner::new();
    let signature = ConcreteSignature {
        params: vec![interner.intern(MonoTy::String)],
        param_tuple_spelled: vec![false],
        result: Some(interner.intern(MonoTy::Float)),
    };
    let err = check_tier0_signature_concrete(&interner, &signature, Span::default())
        .expect_err("a non-tier-0 concrete parameter passed the signature check");
    assert!(err.message.contains("string"), "unexpected error: {err:?}");
}

/// The unfuck's flipped assertion: a MISSED int->float coercion sink reaches
/// the emitter as a raw int-typed value in a float slot and must be a HARD
/// emit error, never silent driver-dependent GLSL. Here we lower
/// `f(i: int) -> float { return i }` (whose return sink DOES coerce) and then
/// strip the `IntToFloat` to simulate a missed sink.
#[test]
fn missed_sink_is_a_hard_emit_error() {
    let (ws, mid) = build_ws("@pure fn f(i: int) -> float { return i }");
    let fref = resolve_root(&ws, mid, "f").expect("root");
    let (scx, mut mf) = lower_test_instance(&ws, fref);

    // The coercion is present today (regression pin for the return sink).
    assert!(
        mf.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::IntToFloat(_))),
        "expected an IntToFloat coercion at the return sink"
    );

    // Strip it: rewrite each `return` to the raw int operand.
    let bids: Vec<_> = mf.blocks.iter().map(|(b, _)| b).collect();
    for bid in bids {
        if let Term::Return(vs) = mf.blocks[bid].term.clone() {
            let stripped: Vec<_> = vs
                .iter()
                .map(|&v| match mf.insts[v] {
                    Inst::IntToFloat(x) => x,
                    _ => v,
                })
                .collect();
            mf.set_term(bid, Term::Return(stripped));
        }
    }

    let r = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf);
    let msg = r
        .expect_err("a missed int->float sink must be a hard emit error")
        .message;
    assert!(
        msg.contains("missed lowering sink"),
        "unexpected error: {msg}"
    );
}

/// **A generic callee with no recorded instantiation is a REJECTION.**
///
/// Reading an absent `spec_bindings` entry as "no instantiation" is right
/// for a callee that has no generics and wrong for one that does: it
/// silently substitutes the declaration's shape, which GLSL has no boxed
/// representation for. Removing the record must reject with a span, never
/// emit against the declaration.
///
/// The CONTROL is asserted first: with the record present the same body
/// emits and names the specialized callee, so the rejection below measures
/// the missing record and not an unemittable fixture.
#[test]
fn a_generic_callee_with_no_recorded_instantiation_rejects() {
    let (ws, mid) = build_ws(
        "@pure fn passthrough<T>(x: T) -> T { return x }\n             @pure fn caller(x: float) -> float { return passthrough(x) }",
    );
    let fref = resolve_root(&ws, mid, "caller").expect("root");
    let (scx, mut mf) = lower_test_instance(&ws, fref);

    // CONTROL: the record exists and the body emits against it.
    assert_eq!(
        mf.spec_bindings.len(),
        1,
        "the fixture recorded no instantiation to remove"
    );
    let ok = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
        .expect("the body emits with its instantiation recorded");
    assert!(ok.contains("passthrough_T_spec_p_float("), "{ok}");

    // Remove exactly the record, change nothing else.
    mf.spec_bindings.clear();
    let msg = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
        .expect_err("a generic callee with no instantiation must reject")
        .message;
    assert!(
        msg.contains("no recorded instantiation") && msg.contains("passthrough"),
        "the rejection does not name the callee or the cause: {msg}"
    );
}

/// **ADMISSION CONTROL for the rule above.** A callee that declares no
/// generics has no record to miss, so it must pass the same gate and emit.
/// Without this the rejection and "GLSL stopped emitting calls" look alike.
#[test]
fn a_nongeneric_callee_needs_no_recorded_instantiation() {
    let (ws, mid) = build_ws(
        "@pure fn doubled(x: float) -> float { return x * 2.0 }\n             @pure fn caller(x: float) -> float { return doubled(x) }",
    );
    let fref = resolve_root(&ws, mid, "caller").expect("root");
    let (scx, mf) = lower_test_instance(&ws, fref);

    // The fixture really does exercise the gate: a call is present, and it
    // carries NO record -- which is the input the generic arm rejects.
    assert!(
        mf.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })),
        "the fixture has no call to admit"
    );
    assert!(
        mf.spec_bindings.is_empty(),
        "a non-generic callee recorded an instantiation"
    );

    let out = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
        .expect("a non-generic callee must be admitted with no record");
    assert!(out.contains("doubled_float("), "{out}");
}

/// A legacy-spelled type: its key is already its symbol.
fn nominal(name: &str) -> NominalType {
    NominalType {
        key: Ustr::from(name),
        symbol: Ustr::from(name),
    }
}

/// A fragment-only interface with an engine group, a stage group, an array
/// group and two samplers -- one of every physical entry, so each arm below
/// perturbs exactly one of them.
fn abi_fixture() -> ShaderInterfacePlan {
    use skull_mir::gpu_layout::ArrayMemberDecl;
    use skull_mir::shader_ir::{LogicalArrayGroup, LogicalTextureGroup, LogicalUniformGroup};
    ShaderInterfacePlan {
        stages: vec![IrStage::Fragment],
        uniform_groups: vec![
            LogicalUniformGroup {
                role: UniformRole::Engine,
                struct_name: nominal("EngineU"),
                members: vec![(Ustr::from("time"), WireKind::F32)],
            },
            LogicalUniformGroup {
                role: UniformRole::Stage(IrStage::Fragment),
                struct_name: nominal("FragU"),
                members: vec![(Ustr::from("cutoff"), WireKind::F32)],
            },
        ],
        array_groups: vec![LogicalArrayGroup {
            stage: IrStage::Fragment,
            owner: Ustr::from("ff"),
            members: vec![ArrayMemberDecl {
                name: Ustr::from("lights"),
                elem: WireKind::F32x4,
                capacity: 4,
                span: Span::default(),
            }],
        }],
        textures: Some(LogicalTextureGroup {
            struct_name: Some(nominal("Tex")),
            samplers: vec![
                (Ustr::from("a"), SamplerKind::Float),
                (Ustr::from("b"), SamplerKind::Float),
            ],
            stages: vec![IrStage::Fragment],
        }),
        ..Default::default()
    }
}

/// **The ABI is validated against the interface it claims to answer, and a
/// dropped declaration is impossible.**
///
/// Each arm perturbs ONE physical entry. A missing declaration surfaces as
/// a driver compile error with no compiler diagnostic, so "omit it and
/// carry on" is the one behaviour that must not exist.
#[test]
fn the_abi_validation_rejects_every_disagreement_with_the_interface() {
    let iface = abi_fixture();
    let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("the fixture must lay out");

    // CONTROL: the unperturbed pair validates and resolves with every entry
    // present, so each rejection below measures its perturbation.
    validate_abi(&iface, &abi, Span::default()).expect("the unperturbed plan must validate");
    let program = ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
        .expect("the unperturbed plan must resolve");
    let ok = program
        .stage(IrStage::Fragment)
        .expect("the fragment projection");
    assert!(ok.uniform_block.is_some() && ok.engine_block.is_some());
    assert!(ok.array_block.is_some());
    assert_eq!(ok.samplers.len(), 2);

    let reject = |a: &ShaderAbiPlan, what: &str| -> String {
        validate_abi(&iface, a, Span::default())
            .err()
            .unwrap_or_else(|| panic!("{what} must reject"))
            .message
    };

    // MISSING stage block.
    let mut m = abi.clone();
    m.uniform_blocks
        .retain(|(b, _)| *b != UNIFORM_BLOCK_BINDING_FRAGMENT);
    assert!(reject(&m, "a missing stage block").contains("has no block for"));

    // MISSING @engine block -- silently omitted before.
    let mut m = abi.clone();
    m.uniform_blocks
        .retain(|(b, _)| *b != UNIFORM_BLOCK_BINDING_ENGINE);
    assert!(reject(&m, "a missing @engine block").contains("EngineU"));

    // MISSING array block, over an interface that DOES declare one -- the
    // arm a fixture without an array group cannot reach.
    let mut m = abi.clone();
    m.uniform_array_blocks.clear();
    assert!(reject(&m, "a missing array block").contains("array group"));

    // WRONG BLOCK at the right binding: presence is not identity. The
    // engine block sits at binding 2, so every per-binding presence check
    // passes while the emitted members are another group's.
    let mut m = abi.clone();
    let engine = m
        .uniform_blocks
        .iter()
        .find(|(b, _)| *b == UNIFORM_BLOCK_BINDING_ENGINE)
        .map(|(_, p)| p.clone())
        .expect("the engine block");
    for (b, p) in m.uniform_blocks.iter_mut() {
        if *b == UNIFORM_BLOCK_BINDING_FRAGMENT {
            *p = engine.clone();
        }
    }
    let msg = reject(&m, "a foreign block at the right binding");
    assert!(msg.contains("EngineU") && msg.contains("FragU"), "{msg}");

    // WRONG MEMBERS in the right block: the name matches and the layout
    // does not.
    let mut m = abi.clone();
    for (b, p) in m.uniform_blocks.iter_mut() {
        if *b == UNIFORM_BLOCK_BINDING_FRAGMENT {
            p.plan.members[0].name = Ustr::from("renamed");
        }
    }
    assert!(reject(&m, "a renamed member").contains("but the interface declares"));

    // WRONG NAME on the right block: the declaration and the block wire
    // spell `name`, and the semantic wire spells the group's symbol, so the
    // runtime's by-name join would miss.
    let mut m = abi.clone();
    for (b, p) in m.uniform_blocks.iter_mut() {
        if *b == UNIFORM_BLOCK_BINDING_FRAGMENT {
            p.name = Ustr::from("FragU_renamed");
        }
    }
    assert!(reject(&m, "a renamed block").contains("FragU_renamed"));

    // WRONG CAPACITY in an array block -- what an `@size(N)` change moves,
    // and what a block-name check alone cannot see.
    let mut m = abi.clone();
    m.uniform_array_blocks[0].1.members[0].capacity = 8;
    assert!(reject(&m, "a changed array capacity").contains("interface declares"));

    // UNKNOWN binding: no interface group claims it, and it belongs to no
    // stage, so only a program-wide check can refuse it.
    let mut m = abi.clone();
    m.uniform_blocks.push((17, engine.clone()));
    assert!(reject(&m, "an unknown binding").contains("which no"));

    // DUPLICATE binding.
    let mut m = abi.clone();
    let d = m
        .uniform_blocks
        .iter()
        .find(|(b, _)| *b == UNIFORM_BLOCK_BINDING_FRAGMENT)
        .cloned()
        .expect("the stage block");
    m.uniform_blocks.push(d);
    assert!(reject(&m, "a duplicated block").contains("more than one"));
}

/// **The sampler join is a BIJECTION**, in all three directions: a
/// duplicated unit, a unit for a sampler the group does not name, and a
/// named sampler with no unit.
///
/// `[a, a]` against an interface naming `[a, b]` has the right length and
/// every element is a member -- and it declares `a` twice while `b` is
/// never declared at all.
#[test]
fn the_sampler_join_is_a_bijection() {
    let iface = abi_fixture();
    let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");
    assert_eq!(abi.texture_units, vec![Ustr::from("a"), Ustr::from("b")]);
    assert_eq!(
        ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
            .expect("the unperturbed join must resolve")
            .stage(IrStage::Fragment)
            .expect("the fragment projection")
            .samplers
            .len(),
        2
    );

    let reject = |units: Vec<Ustr>, what: &str| -> String {
        let mut a = abi.clone();
        a.texture_units = units;
        validate_abi(&iface, &a, Span::default())
            .err()
            .unwrap_or_else(|| panic!("{what} must reject"))
            .message
    };
    assert!(
        reject(vec![Ustr::from("a"), Ustr::from("a")], "a duplicated unit")
            .contains("more than one unit")
    );
    assert!(
        reject(vec![Ustr::from("a"), Ustr::from("zzz")], "an alien unit").contains("does not name")
    );
    // The MISSING branch: fewer units than the group names.
    assert!(reject(vec![Ustr::from("a")], "a sampler with no unit").contains("gives no unit"));
}

/// **Two attributes may not share a location.** A bijection on NAMES
/// accepts `[(a, 0), (b, 0)]`, which aliases one buffer slot onto two
/// inputs and is invisible until the geometry is wrong.
#[test]
fn overlapping_attribute_locations_reject() {
    use skull_mir::gpu_layout::AttrKind;
    use skull_mir::shader_ir::{LogicalVertexInput, VertexRate};
    let iface = ShaderInterfacePlan {
        stages: vec![IrStage::Vertex],
        vertex_inputs: vec![LogicalVertexInput {
            struct_name: nominal("V"),
            rate: VertexRate::PerVertex,
            attrs: vec![
                (Ustr::from("a"), AttrKind::F32x2),
                (Ustr::from("b"), AttrKind::F32x2),
            ],
        }],
        ..Default::default()
    };
    let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");
    // CONTROL: distinct locations validate.
    assert_eq!(
        abi.attribute_locations,
        vec![(Ustr::from("a"), 0), (Ustr::from("b"), 1)]
    );
    validate_abi(&iface, &abi, Span::default()).expect("distinct locations must validate");

    let mut overlap = abi.clone();
    overlap.attribute_locations = vec![(Ustr::from("a"), 0), (Ustr::from("b"), 0)];
    let msg = validate_abi(&iface, &overlap, Span::default())
        .expect_err("overlapping locations must reject")
        .message;
    assert!(msg.contains("both occupy location"), "{msg}");

    // A location at the top of the space must REJECT, not overflow: the
    // validator exists to refuse malformed plans and may not panic or wrap
    // on one.
    let mut top = abi.clone();
    top.attribute_locations = vec![(Ustr::from("a"), u32::MAX), (Ustr::from("b"), 1)];
    let msg = validate_abi(&iface, &top, Span::default())
        .expect_err("a location at u32::MAX must reject")
        .message;
    assert!(msg.contains("spans past the end"), "{msg}");
}

/// **A stage the interface does not declare is a REJECTION, not a lookup
/// miss.**
///
/// `ProgramAbi::of` takes the stages the renderer will emit as a census.
/// Trusting `iface.stages` instead would push the disagreement to the
/// projection lookup, where the only options are a panic or a silent
/// fallback -- and both are the shape this arc removes.
#[test]
fn a_stage_the_interface_does_not_declare_rejects() {
    let mut iface = abi_fixture();
    let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");

    // CONTROL: the fixture declares Fragment, and projecting it works.
    assert_eq!(iface.stages, vec![IrStage::Fragment]);
    let program = ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
        .expect("the declared stage must project");
    assert!(program.stage(IrStage::Fragment).is_ok());
    // ...and asking for one it never projected is an error, not a panic.
    assert!(program.stage(IrStage::Vertex).is_err());

    // Requiring a stage the interface does not declare rejects up front.
    let msg = ProgramAbi::of(&iface, &abi, &[IrStage::Vertex], Span::default())
        .err()
        .expect("an undeclared stage must reject")
        .message;
    assert!(
        msg.contains("vertex") && msg.contains("does not declare"),
        "{msg}"
    );

    // ...and once the interface declares it, the same request succeeds --
    // so the rejection is about the census and not about Vertex.
    iface.stages.push(IrStage::Vertex);
    ProgramAbi::of(&iface, &abi, &[IrStage::Vertex], Span::default())
        .expect("a declared stage must project");
}

/// **Two logical groups claiming one binding is the INTERFACE's defect.**
///
/// Normal construction turns it into a duplicate ABI entry and it is caught
/// downstream, which makes the diagnostic about the wrong side. The
/// validator states it directly, for both group kinds.
#[test]
fn duplicate_expected_bindings_reject() {
    use skull_mir::gpu_layout::ArrayMemberDecl;
    use skull_mir::shader_ir::{LogicalArrayGroup, LogicalUniformGroup};

    // CONTROL: one group per role validates.
    let base = abi_fixture();
    let abi = ShaderAbiPlan::glsl(&base, Span::default()).expect("lay out");
    validate_abi(&base, &abi, Span::default()).expect("the control must validate");

    let mut two_roles = base.clone();
    two_roles.uniform_groups.push(LogicalUniformGroup {
        role: UniformRole::Stage(IrStage::Fragment),
        struct_name: nominal("OtherU"),
        members: vec![(Ustr::from("x"), WireKind::F32)],
    });
    let msg = validate_abi(&two_roles, &abi, Span::default())
        .expect_err("two groups at one role must reject")
        .message;
    assert!(msg.contains("two uniform groups at binding"), "{msg}");

    let mut two_arrays = base.clone();
    two_arrays.array_groups.push(LogicalArrayGroup {
        stage: IrStage::Fragment,
        owner: Ustr::from("other"),
        members: vec![ArrayMemberDecl {
            name: Ustr::from("y"),
            elem: WireKind::F32x4,
            capacity: 2,
            span: Span::default(),
        }],
    });
    let msg = validate_abi(&two_arrays, &abi, Span::default())
        .expect_err("two array groups for one stage must reject")
        .message;
    assert!(msg.contains("two array groups"), "{msg}");
}

/// **A fragment stage with no declared colour output rejects**, rather than
/// having a `vec4` invented for an inconsistent plan.
#[test]
fn a_fragment_stage_with_no_declared_output_rejects() {
    use skull_mir::shader_ir::FragmentOutput;
    let (ws, mid) = build_ws(
        "value Vary { position: Vec4, uv: Vec2 }\n\
         value QuadVert { pos: Vec2, texcoord: Vec2 }\n\
         @shader(vertex)\n\
         fn ov(@attr a: QuadVert) -> Vary { return Vary { position: v(a.pos.x, a.pos.y, 0.0, 1.0), uv: a.texcoord } }\n\
         @shader(fragment)\n\
         fn of(vary: Vary) -> Vec4 { return v(vary.uv.x, 0.0, 0.0, 1.0) }\n\
         fn main() { let k = rock_shader(ov, of) }",
    );
    let vf = resolve_root(&ws, mid, "ov").expect("root");
    let ff = resolve_root(&ws, mid, "of").expect("root");
    let session = shader_program_session(
        &ws,
        ShaderProgram {
            vert: Some(vf),
            frag: ff,
        },
    )
    .expect("the fixture must realize");
    let mut plan = plan_shader_pipeline_from_view(&ws, &session.scx, &session.view)
        .expect("the fixture must plan");

    // CONTROL: it renders with the output declared.
    assert!(matches!(
        plan.interface.fragment_output,
        FragmentOutput::Single(_)
    ));
    render_shader_pipeline(&ws, session.scx.interner(), &plan, GlslProfile::Core330)
        .expect("the control must render");

    plan.interface.fragment_output = FragmentOutput::None;
    let msg = render_shader_pipeline(&ws, session.scx.interner(), &plan, GlslProfile::Core330)
        .err()
        .expect("a fragment stage with no output must reject")
        .message;
    assert!(msg.contains("no fragment output"), "{msg}");
}

/// **A key of the WRONG ARITY rejects.**
///
/// The key is positional against the callee's declared generics, so a key
/// of a different length names a different function's instantiation.
/// `spec_subst_from_key` zips the two, which silently drops the tail and
/// substitutes the head -- a body emitted at a shape nobody asked for, with
/// no complaint. Length is checked, not assumed.
///
/// Both directions, and the SHORT case is non-empty on purpose: an empty
/// key is already refused as "no recorded instantiation", so a `vec![]`
/// fixture would pass against the missing length check and prove nothing.
/// That is why the callee declares TWO generics.
#[test]
fn a_generic_callee_with_a_wrong_arity_key_rejects() {
    use skull_mir::monoty::MonoTy;
    let (ws, mid) = build_ws(
        "@pure fn pick<A, B>(a: A, b: B) -> A { return a }\n\
         @pure fn caller(x: float) -> float { return pick(x, 1) }",
    );
    let fref = resolve_root(&ws, mid, "caller").expect("root");
    let (scx, base) = lower_test_instance(&ws, fref);

    // CONTROL: the recorded key has the declared arity, and the body emits.
    let iid = *base
        .spec_bindings
        .keys()
        .next()
        .expect("a recorded instantiation");
    assert_eq!(
        base.spec_bindings[&iid].args.len(),
        2,
        "the callee declares two generics"
    );
    emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &base)
        .expect("the fixture must emit with a well-formed key");

    for (label, key) in [
        ("too long", vec![MonoTy::Float, MonoTy::Int, MonoTy::Int]),
        // Non-empty and short: the case an `is_empty` check cannot see.
        ("too short", vec![MonoTy::Float]),
    ] {
        let mut mf = base.clone();
        let callee = base.spec_bindings[&iid].func;
        mf.spec_bindings
            .insert(iid, Instance::intern(scx.interner(), callee, key));
        let msg = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
            .err()
            .unwrap_or_else(|| panic!("a {label} key must reject"))
            .message;
        assert!(
            msg.contains("generic") && msg.contains("pick"),
            "the {label} rejection does not name the callee: {msg}"
        );
    }
}

#[test]
fn production_shader_body_consumes_the_concrete_result_plan() {
    let (ws, mid) = build_ws(
        "fn passthrough<T>(x: T) -> T { return x }\n\
         fn caller(x: float) -> float { return passthrough(x) }",
    );
    let caller = resolve_root(&ws, mid, "caller").expect("root");
    let session = shader_library_session(&ws, vec![caller]).expect("shader session");
    let mut view = session.view.clone();
    let passthrough = view
        .order
        .iter()
        .find(|inst| ws.module(inst.func.module).functions[inst.func.func].name == "passthrough")
        .cloned()
        .expect("generic helper is retained");
    let plan = Arc::make_mut(&mut view.concrete_plans)
        .get_mut(&passthrough)
        .expect("generic helper has a concrete plan");
    std::sync::Arc::make_mut(&mut Arc::make_mut(plan).signature).result =
        Some(session.scx.interner().intern(MonoTy::String));

    let err = emit_shader_library_with_view(&ws, &session.scx, &view)
        .expect_err("a concrete result plan disagreement must reject GLSL emission");
    assert!(
        err.message.contains("return type `string`"),
        "unexpected error: {}",
        err.message
    );
}

#[test]
fn production_shader_calls_use_concrete_signature_and_instance_records() {
    let (ws, mid) = build_ws(
        "fn passthrough<T>(x: T) -> T { return x }\n\
         fn caller(x: float) -> float { return passthrough(x) }",
    );
    let caller = resolve_root(&ws, mid, "caller").expect("root");
    let session = shader_library_session(&ws, vec![caller]).expect("shader session");
    let caller_inst = session.view.roots.first().expect("caller root").clone();
    let caller_plan = session
        .view
        .concrete_plans
        .get(&caller_inst)
        .expect("caller has a concrete plan");
    assert!(
        !caller_plan.callees.is_empty(),
        "call signature population is empty"
    );

    let scx = &session.scx;
    let mut body = skull_mir::lower::lower_instance_with_plans(
        &ws,
        scx,
        caller_plan,
        &session.view.concrete_plans,
    )
    .expect("caller lowering");
    let call = body
        .insts
        .iter()
        .find_map(|(iid, inst)| matches!(inst, Inst::Call { .. }).then_some(iid))
        .expect("caller has a direct call");
    assert!(
        body.callee_instances.contains_key(&call),
        "concrete call has no instance record"
    );
    body.callee_instances.clear();
    let missing_owner = emit_fn_def_with(
        &ws,
        scx.interner(),
        &ShaderInstance::new(caller_inst.clone()),
        &body,
        None,
        Some(caller_plan),
    )
    .expect_err("a concrete call without its instance record must reject");
    assert!(
        missing_owner.message.contains("no recorded instantiation"),
        "{missing_owner}"
    );

    let mut view = session.view.clone();
    let caller_plan = Arc::make_mut(&mut view.concrete_plans)
        .get_mut(&caller_inst)
        .expect("caller plan");
    let callee = Arc::make_mut(caller_plan)
        .callees
        .values_mut()
        .next()
        .expect("call record");
    Arc::make_mut(&mut callee.signature).result = Some(scx.interner().intern(MonoTy::String));
    let bad_signature = emit_shader_library_with_view(&ws, scx, &view)
        .expect_err("a non-tier-0 concrete call result must reject GLSL emission");
    assert!(
        bad_signature
            .message
            .contains("concrete callee signature disagrees with its body plan"),
        "{bad_signature}"
    );
}

// A partial key -- a recorded instantiation with an unreified slot -- used to
// be pinned here as its own rejection. A key is TOTAL now, so the producer
// returns no key at all rather than one with a hole, and the state arrives
// as an absent record. That is
// `a_generic_callee_with_no_recorded_instantiation_rejects` above, which is
// the surviving coverage; a second test inserting an empty key would assert
// the same arm twice.
