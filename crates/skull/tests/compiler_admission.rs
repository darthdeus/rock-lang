//! Recovery keeps a usable tree; the compile entry points still refuse it.

use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::ModuleId;
use skull::mir_emit::MirEmit;
use skull::pipeline::{self, CompileHost, EmitTarget, Phase, Stage};
use skull_parser::stdlib::ModuleSet;
use skull_parser::{Binding, CompilerError, ItemKind, StatementKind};
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
    assert_eq!(failure.stage, Stage::Check, "{failure:?}");
    assert_eq!(failure.errors.len(), 1, "{failure:?}");
    let error = &failure.errors[0];
    assert!(error.message.contains(expected), "{failure:?}");
    assert_eq!(error.location.file, Ustr::from("admission.rock"));
    assert_eq!(host.reached, reached, "source: {src}");
}

#[test]
fn recovered_statement_preserves_the_tree_but_stops_before_aliases() {
    let src = "fn main() {\n let broken =\n let kept = 7\n print(kept)\n}\n";
    let ast = skull_parser::parse_module(src, Ustr::from("admission.rock"))
        .expect("recoverable syntax returns a tree");
    assert_eq!(ast.recovery_errors.len(), 1, "{:?}", ast.recovery_errors);
    let ItemKind::Function(function) = &ast.items[0].kind else {
        panic!("main survives statement recovery");
    };
    let body = &ast.blocks[function.body.rock_block().unwrap()];
    assert_eq!(body.statements.len(), 2, "{body:?}");
    let StatementKind::Let(binding) = &ast.stmts[body.statements[0]].kind else {
        panic!("the valid binding survives recovery");
    };
    assert!(matches!(&binding.binding, Binding::Ident(name) if name.name == "kept"));
    rejected(src, &ast.recovery_errors[0].message, &["parsed"]);
}

#[test]
fn assembled_modules_report_all_recovered_errors_in_module_order() {
    let first = skull_parser::parse_module("fn first(\n", Ustr::from("first.rock"))
        .expect("recoverable first module");
    let second = skull_parser::parse_module("fn second(\n", Ustr::from("second.rock"))
        .expect("recoverable second module");
    assert_eq!(first.recovery_errors.len(), 1);
    assert_eq!(second.recovery_errors.len(), 1);
    let mut host = RecordingHost::default();
    let modules = pipeline::with_stdlib([first, second]).expect("stdlib");
    let failure = match pipeline::compile_modules(
        modules,
        Ustr::from("first.rock"),
        EmitTarget::SINGLE_SOURCE,
        &mut host,
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("recovered modules compiled"),
    };
    assert_eq!(failure.stage, Stage::Check);
    let files: Vec<_> = failure.errors.iter().map(|e| e.location.file).collect();
    assert_eq!(files, [Ustr::from("first.rock"), Ustr::from("second.rock")]);
    assert!(host.reached.is_empty(), "{:?}", host.reached);
}

#[test]
fn unknown_top_level_macro_cannot_silently_disappear() {
    let src = "missing!()\nfn main() {}\n";
    let ast = skull_parser::parse_module_clean(src, Ustr::from("admission.rock"))
        .expect("source syntax is clean before expansion");
    assert!(matches!(
        ast.items[0].kind,
        ItemKind::MacroInvocation { .. }
    ));
    rejected(
        src,
        "no macro named `missing` exists",
        &["parsed", "aliases"],
    );
}

#[test]
fn macro_generated_import_cannot_silently_disappear() {
    let src = "macro imports() { use absent::item; }\nimports!()\nfn main() {}\n";
    skull_parser::parse_module_clean(src, Ustr::from("admission.rock"))
        .expect("source syntax is clean before expansion");
    rejected(
        src,
        "macro-generated `mod`/`use` is not supported",
        &["parsed", "aliases"],
    );
}

#[test]
fn expression_macro_error_precedes_its_unexpanded_call_error() {
    let src = "macro declares() { fn generated() {} }\nfn main() { declares!() }\n";
    skull_parser::parse_module_clean(src, Ustr::from("admission.rock"))
        .expect("source syntax is clean before expansion");
    rejected(
        src,
        "items declared by macro `declares` cannot be emitted where it is used as an expression",
        &["parsed", "aliases"],
    );
}

#[test]
fn macro_argument_labels_are_checked_before_expansion_discards_them() {
    let src = "macro identity(value: expr) { value }\nfn main() { identity(wrong: 7) }\n";
    skull_parser::parse_module_clean(src, Ustr::from("admission.rock"))
        .expect("source syntax is clean before expansion");
    rejected(
        src,
        "macro `identity` has no parameter named `wrong`",
        &["parsed", "aliases"],
    );
}

#[test]
fn macro_function_collision_is_checked_before_macro_definitions_disappear() {
    let src =
        "macro conflict() { 7 }\nfn conflict() -> int { 8 }\nfn main() { print(conflict()) }\n";
    skull_parser::parse_module_clean(src, Ustr::from("admission.rock"))
        .expect("source syntax is clean before expansion");
    rejected(
        src,
        "macro `conflict` collides with a function named `conflict`",
        &["parsed", "aliases"],
    );
}

#[test]
fn tokenizer_failure_stops_before_any_host_phase() {
    let mut host = RecordingHost::default();
    let failure = match pipeline::compile_source(
        "admission.rock",
        "fn main() { print(\"unterminated)",
        EmitTarget::SINGLE_SOURCE,
        &mut host,
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("unterminated string compiled"),
    };
    assert_eq!(failure.stage, Stage::Parse);
    assert_eq!(failure.errors.len(), 1);
    assert!(host.reached.is_empty(), "{:?}", host.reached);
}

#[test]
fn repaired_source_and_valid_macros_reach_real_emission() {
    for src in [
        "fn main() { let kept = 7\n print(kept) }\n",
        "macro declares() { fn generated() -> int { 7 } }\ndeclares!()\nfn main() { print(generated()) }\n",
        "macro identity(value: expr) { value }\nfn main() { print(identity(value: 7)) }\n",
    ] {
        let mut host = RecordingHost::default();
        let compiled =
            pipeline::compile_source("admission.rock", src, EmitTarget::SINGLE_SOURCE, &mut host)
                .unwrap_or_else(|failure| panic!("valid control failed: {failure:?}\n{src}"));
        assert!(compiled.emission.lua.contains("print("));
        assert_eq!(
            host.reached,
            [
                "parsed", "aliases", "checked", "comptime", "lints", "report", "prepared", "emit"
            ],
        );
    }
}
