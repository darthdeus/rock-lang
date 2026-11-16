mod generators;
mod harness;

use clap::{Arg, Command};
use generators::*;
use harness::{print_results, TypeTest};

type_test!(HoverTest, Hover, gen_hover);
type_test!(DiagnosticTest, Diagnostic, gen_diagnostic);
type_test!(InlayHintTest, InlayHint, gen_inlay_hint);
type_test!(CodeActionTest, CodeAction, gen_code_action);
type_test!(SymbolKindTest, SymbolKind, gen_int_newtype);
type_test!(DiagnosticSeverityTest, DiagnosticSeverity, gen_int_newtype);
type_test!(CompletionItemKindTest, CompletionItemKind, gen_int_newtype);
type_test!(
    PrepareRenameResponseTest,
    PrepareRenameResponse,
    gen_prepare_rename_response
);
type_test!(SemanticTokensTest, SemanticTokens, gen_semantic_tokens);

type_test!(
    RequestIdTest,
    RequestId,
    lsp_server_orig::RequestId,
    lsp_server_fork::RequestId,
    gen_request_id
);
type_test!(
    ResponseErrorTest,
    ResponseError,
    lsp_server_orig::ResponseError,
    lsp_server_fork::ResponseError,
    gen_response_error
);
type_test!(
    LspRequestTest,
    Request,
    lsp_server_orig::Request,
    lsp_server_fork::Request,
    gen_lsp_request
);
type_test!(
    LspResponseTest,
    Response,
    lsp_server_orig::Response,
    lsp_server_fork::Response,
    gen_lsp_response
);
type_test!(
    LspNotificationTest,
    Notification,
    lsp_server_orig::Notification,
    lsp_server_fork::Notification,
    gen_lsp_notification
);
type_test!(
    LspMessageTest,
    Message,
    lsp_server_orig::Message,
    lsp_server_fork::Message,
    gen_lsp_message
);

fn all_tests() -> Vec<Box<dyn TypeTest>> {
    vec![
        Box::new(HoverTest),
        Box::new(DiagnosticTest),
        Box::new(InlayHintTest),
        Box::new(CodeActionTest),
        Box::new(SymbolKindTest),
        Box::new(DiagnosticSeverityTest),
        Box::new(CompletionItemKindTest),
        Box::new(PrepareRenameResponseTest),
        Box::new(SemanticTokensTest),
        Box::new(RequestIdTest),
        Box::new(ResponseErrorTest),
        Box::new(LspRequestTest),
        Box::new(LspResponseTest),
        Box::new(LspNotificationTest),
        Box::new(LspMessageTest),
    ]
}

fn main() {
    let matches = Command::new("lsp-serde-test")
        .about("A/B test: original lsp-types serde vs. manual fork")
        .subcommand(
            Command::new("run")
                .about("Run compatibility tests")
                .arg(
                    Arg::new("type")
                        .long("type")
                        .short('t')
                        .help("Run only this type (partial match)"),
                )
                .arg(
                    Arg::new("count")
                        .long("count")
                        .short('n')
                        .default_value("100")
                        .help("Iterations per type"),
                )
                .arg(
                    Arg::new("seed")
                        .long("seed")
                        .default_value("42")
                        .help("RNG seed"),
                )
                .arg(
                    Arg::new("verbose")
                        .long("verbose")
                        .short('v')
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(Command::new("list").about("List available type tests"))
        .get_matches();

    match matches.subcommand() {
        Some(("list", _)) => {
            for t in all_tests() {
                println!("  {}", t.type_name());
            }
        }
        Some(("run", m)) => {
            let count: usize = m
                .get_one::<String>("count")
                .unwrap()
                .parse()
                .expect("count must be a number");
            let seed: u64 = m
                .get_one::<String>("seed")
                .unwrap()
                .parse()
                .expect("seed must be a number");
            let verbose = m.get_flag("verbose");
            let filter = m.get_one::<String>("type").map(|s| s.as_str());

            let tests: Vec<Box<dyn TypeTest>> = all_tests()
                .into_iter()
                .filter(|t| {
                    filter.is_none_or(|f| t.type_name().to_lowercase().contains(&f.to_lowercase()))
                })
                .collect();

            if tests.is_empty() {
                eprintln!("No tests matched filter {:?}", filter);
                std::process::exit(1);
            }

            let results: Vec<_> = tests.iter().map(|t| t.run(count, seed)).collect();
            print_results(&results, verbose);
        }
        _ => {
            eprintln!("Use `lsp-serde-test run` or `lsp-serde-test list`");
            std::process::exit(1);
        }
    }
}
