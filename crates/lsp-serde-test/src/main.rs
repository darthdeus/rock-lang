mod generators;
mod harness;

use clap::{Arg, Command};
use generators::*;
use harness::{print_results, TypeTest};

type OrigTagSupport = lsp_types::TagSupport<lsp_types::CompletionItemTag>;
type ForkTagSupport = lsp_types_manual::TagSupport<lsp_types_manual::CompletionItemTag>;

type_test!(
    HoverClientCapabilitiesTest,
    HoverClientCapabilities,
    gen_hover_client_capabilities
);
type_test!(HoverTest, Hover, gen_hover);
type_test!(
    SemanticTokensFullOptionsTest,
    SemanticTokensFullOptions,
    gen_semantic_tokens_full_options
);
type_test!(
    SemanticTokensOptionsTest,
    SemanticTokensOptions,
    gen_semantic_tokens_options
);
type_test!(
    SemanticTokensServerCapabilitiesTest,
    SemanticTokensServerCapabilities,
    gen_semantic_tokens_server_capabilities
);
type_test!(
    CompletionOptionsTest,
    CompletionOptions,
    gen_completion_options
);
type_test!(
    InitializeParamsTest,
    InitializeParams,
    gen_initialize_params
);
type_test!(
    CompletionItemCapabilityTest,
    CompletionItemCapability,
    gen_completion_item_capability
);
type_test!(
    DocumentSymbolClientCapabilitiesTest,
    DocumentSymbolClientCapabilities,
    gen_document_symbol_client_capabilities
);
type_test!(
    WorkspaceSymbolClientCapabilitiesTest,
    WorkspaceSymbolClientCapabilities,
    gen_workspace_symbol_client_capabilities
);
type_test!(DiagnosticTest, Diagnostic, gen_diagnostic);
type_test!(InlayHintTest, InlayHint, gen_inlay_hint);
type_test!(WorkspaceSymbolTest, WorkspaceSymbol, gen_workspace_symbol);
type_test!(
    InitializeResultTest,
    InitializeResult,
    gen_initialize_result
);
type_test!(
    CodeActionOrCommandTest,
    CodeActionOrCommand,
    gen_code_action_or_command
);
type_test!(SymbolKindTest, SymbolKind, gen_int_newtype);
type_test!(DiagnosticSeverityTest, DiagnosticSeverity, gen_int_newtype);
type_test!(CompletionItemKindTest, CompletionItemKind, gen_int_newtype);
type_test!(
    FormattingOptionsTest,
    FormattingOptions,
    gen_formatting_options
);
type_test!(
    DocumentOnTypeFormattingRegistrationOptionsTest,
    DocumentOnTypeFormattingRegistrationOptions,
    gen_document_on_type_formatting_registration_options
);
type_test!(
    MessageActionItemTest,
    MessageActionItem,
    gen_message_action_item
);
type_test!(
    PrepareRenameResponseTest,
    PrepareRenameResponse,
    gen_prepare_rename_response
);
type_test!(WatchKindTest, WatchKind, gen_watch_kind);
type_test!(
    PositionEncodingKindTest,
    PositionEncodingKind,
    gen_position_encoding_kind
);
type_test!(SemanticTokensTest, SemanticTokens, gen_semantic_tokens);
type_test!(
    TagSupportTest,
    TagSupport,
    OrigTagSupport,
    ForkTagSupport,
    gen_tag_support
);

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
        Box::new(HoverClientCapabilitiesTest),
        Box::new(HoverTest),
        Box::new(SemanticTokensFullOptionsTest),
        Box::new(SemanticTokensOptionsTest),
        Box::new(SemanticTokensServerCapabilitiesTest),
        Box::new(CompletionOptionsTest),
        Box::new(InitializeParamsTest),
        Box::new(CompletionItemCapabilityTest),
        Box::new(DocumentSymbolClientCapabilitiesTest),
        Box::new(WorkspaceSymbolClientCapabilitiesTest),
        Box::new(DiagnosticTest),
        Box::new(InlayHintTest),
        Box::new(WorkspaceSymbolTest),
        Box::new(InitializeResultTest),
        Box::new(CodeActionOrCommandTest),
        Box::new(SymbolKindTest),
        Box::new(DiagnosticSeverityTest),
        Box::new(CompletionItemKindTest),
        Box::new(FormattingOptionsTest),
        Box::new(DocumentOnTypeFormattingRegistrationOptionsTest),
        Box::new(MessageActionItemTest),
        Box::new(PrepareRenameResponseTest),
        Box::new(WatchKindTest),
        Box::new(PositionEncodingKindTest),
        Box::new(SemanticTokensTest),
        Box::new(TagSupportTest),
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
