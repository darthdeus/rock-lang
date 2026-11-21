use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::MirEmit;
use skull::pipeline::{self, CompileHost, EmitTarget, Phase, Stage};
use skull_parser::stdlib::ModuleSet;
use skull_parser::{CompilerError, ItemKind, Pattern, StatementKind};
use ustr::Ustr;

#[derive(Default)]
struct RecordingHost {
    reached: Vec<&'static str>,
}

impl CompileHost for RecordingHost {
    fn aliases(&mut self, _modules: &ModuleSet) -> Result<NsAliases, CompilerError> {
        self.reached.push("aliases");
        Ok(NsAliases::default())
    }

    fn phase(&mut self, phase: Phase<'_>) {
        self.reached.push(match phase {
            Phase::Parsed => "parsed",
            Phase::Checked(_) => "checked",
            Phase::Prepared => "prepared",
        });
    }

    fn comptime_inputs(&mut self) -> Result<ComptimeInputs, CompilerError> {
        self.reached.push("comptime");
        Ok(ComptimeInputs::default())
    }

    fn lint_findings(&mut self, _ws: &Workspace) -> Vec<bone_lints::Finding> {
        self.reached.push("lints");
        Vec::new()
    }

    fn report_soft_findings(&mut self, _soft: Vec<bone_lints::Finding>) {
        self.reached.push("report");
    }

    fn emit(
        &mut self,
        ws: &Workspace,
        entry: ModuleId,
        target: EmitTarget<'_>,
    ) -> Result<MirEmit, CompilerError> {
        self.reached.push("emit");
        pipeline::emit_program(ws, entry, target)
    }
}

fn rejected(src: &str, expected: &str, reached: &[&str]) {
    let mut host = RecordingHost::default();
    let failure =
        match pipeline::compile_source("admission.rock", src, EmitTarget::SINGLE_SOURCE, &mut host)
        {
            Err(failure) => failure,
            Ok(_) => panic!("invalid source compiled: {src}"),
        };
    assert_eq!(failure.stage, Stage::Check);
    assert_eq!(failure.errors.len(), 1);
    let error = &failure.errors[0];
    assert!(error.message.contains(expected));
    assert_eq!(error.location.file, Ustr::from("admission.rock"));
    assert_eq!(host.reached, reached);
}

#[test]
fn recovered_statement_preserves_the_tree_but_stops_before_aliases() {
    let src = "fn main() {\n let broken =\n let kept = 7\n print(kept)\n}\n";
    let ast = skull_parser::parse_module(src, Ustr::from("admission.rock"))
        .expect("recoverable syntax returns a tree");
    assert_eq!(ast.recovery_errors.len(), 1);
    let ItemKind::Function(function) = &ast.items[0].kind else {
        panic!("main survives statement recovery");
    };
    let body = &ast.blocks[function.body.rock_block().unwrap()];
    assert_eq!(body.statements.len(), 2);
    let StatementKind::Let(binding) = &ast.stmts[body.statements[0]].kind else {
        panic!("the valid binding survives recovery");
    };
    assert!(matches!(&binding.pattern, Pattern::Ident(name) if name.name == "kept"));
    rejected(src, &ast.recovery_errors[0].message, &["parsed"]);
}

#[test]
fn macro_errors_are_reported_before_expansion_discards_their_source() {
    for (src, expected) in [
        (
            "missing!()\nfn main() {}\n",
            "no macro named `missing` exists",
        ),
        (
            "macro imports() { use absent::item; }\nimports!()\nfn main() {}\n",
            "macro-generated `mod`/`use` is not supported",
        ),
        (
            "macro declares() { fn generated() {} }\nfn main() { declares!() }\n",
            "items declared by macro `declares` cannot be emitted where it is used as an expression",
        ),
        (
            "macro identity(value: expr) { value }\nfn main() { identity(wrong: 7) }\n",
            "macro `identity` has no parameter named `wrong`",
        ),
        (
            "macro conflict() { 7 }\nfn conflict() -> int { 8 }\nfn main() { print(conflict()) }\n",
            "macro `conflict` collides with a function named `conflict`",
        ),
    ] {
        rejected(src, expected, &["parsed", "aliases"]);
    }
}

#[test]
fn repeated_test_checks_reuse_stdlib_only_for_independent_sources() {
    use std::sync::atomic::Ordering::Relaxed;

    for (src, reused) in [
        ("fn main() { print([1, 2, 3].len()) }", true),
        (
            "macro local_answer() { 42 } fn main() { print(local_answer()) }",
            false,
        ),
        (
            "value Number { x: int }\noperator +(a: Number, b: Number) -> Number { Number { x: a.x + b.x } }\nfn main() { let _ = Number { x: 1 } + Number { x: 2 } }",
            false,
        ),
    ] {
        let file = Ustr::from("admission.rock");
        let source = skull_parser::parse_module(src, file).unwrap();
        let mut warm = pipeline::with_stdlib([source]).unwrap();
        let mut cold = warm.clone();
        let (cold, cold_errors) =
            pipeline::check_modules(&mut cold, |_| Ok(NsAliases::default())).unwrap();
        let (warm, warm_errors) =
            pipeline::check_test_modules(&mut warm, |_| Ok(NsAliases::default())).unwrap();
        assert!(cold_errors.is_empty() && warm_errors.is_empty(), "{src}");
        let emit = |ws: &Workspace| {
            pipeline::emit_program(ws, ws.module_ids[&file], EmitTarget::SINGLE_SOURCE)
                .unwrap()
                .lua
        };
        assert_eq!(emit(&cold), emit(&warm));
        assert_eq!(
            warm.reinferred_body_count.load(Relaxed) < cold.reinferred_body_count.load(Relaxed),
            reused,
            "{src}",
        );
    }
}
