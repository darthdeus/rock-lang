use crate::{
    DynamicRegistrationClientCapabilities, Range, StaticRegistrationOptions,
    TextDocumentIdentifier, TextDocumentRegistrationOptions, WorkDoneProgressOptions,
    WorkDoneProgressParams,
};

pub type InlineValueClientCapabilities = DynamicRegistrationClientCapabilities;

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum InlineValueServerCapabilities {
    Options(InlineValueOptions),
    RegistrationOptions(InlineValueRegistrationOptions),
}

impl_untagged_enum!(InlineValueServerCapabilities {
    Options(InlineValueOptions),
    RegistrationOptions(InlineValueRegistrationOptions),
});

/// Inline value options used during static registration.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueOptions {
    pub work_done_progress_options: WorkDoneProgressOptions,
}

impl_lsp_struct!(
    InlineValueOptions,
    flatten {
        work_done_progress_options: WorkDoneProgressOptions
    }
);

/// Inline value options used during static or dynamic registration.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueRegistrationOptions {
    pub inline_value_options: InlineValueOptions,
    pub text_document_registration_options: TextDocumentRegistrationOptions,
    pub static_registration_options: StaticRegistrationOptions,
}

impl_lsp_struct!(
    InlineValueRegistrationOptions,
    flatten {
        inline_value_options: InlineValueOptions,
        text_document_registration_options: TextDocumentRegistrationOptions,
        static_registration_options: StaticRegistrationOptions,
    }
);

/// A parameter literal used in inline value requests.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InlineValueParams {
    pub work_done_progress_params: WorkDoneProgressParams,

    /// The text document.
    pub text_document: TextDocumentIdentifier,

    /// The document range for which inline values should be computed.
    pub range: Range,

    /// Additional information about the context in which inline values were
    /// requested.
    pub context: InlineValueContext,
}

impl_lsp_struct!(
    InlineValueParams,
    required {
        text_document: TextDocumentIdentifier = "textDocument",
        range: Range = "range",
        context: InlineValueContext = "context",
    },
    optional {},
    flatten {
        work_done_progress_params: WorkDoneProgressParams
    }
);

/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueContext {
    pub frame_id: i32,
    pub stopped_location: Range,
}

impl_lsp_struct!(
    InlineValueContext,
    required {
        frame_id: i32 = "frameId",
        stopped_location: Range = "stoppedLocation",
    },
    optional {}
);

/// Provide inline value as text.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueText {
    pub range: Range,
    pub text: String,
}

impl_lsp_struct!(
    InlineValueText,
    required {
        range: Range = "range",
        text: String = "text",
    },
    optional {}
);

/// Provide inline value through a variable lookup.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueVariableLookup {
    pub range: Range,
    pub variable_name: Option<String>,
    pub case_sensitive_lookup: bool,
}

impl_lsp_struct!(
    InlineValueVariableLookup,
    required {
        range: Range = "range",
        case_sensitive_lookup: bool = "caseSensitiveLookup",
    },
    optional {
        variable_name: String = "variableName"
    }
);

/// Provide an inline value through an expression evaluation.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueEvaluatableExpression {
    pub range: Range,
    pub expression: Option<String>,
}

impl_lsp_struct!(
    InlineValueEvaluatableExpression,
    required {
        range: Range = "range"
    },
    optional {
        expression: String = "expression"
    }
);

/// The InlineValue types combines all inline value types into one type.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum InlineValue {
    Text(InlineValueText),
    VariableLookup(InlineValueVariableLookup),
    EvaluatableExpression(InlineValueEvaluatableExpression),
}

impl_untagged_enum!(InlineValue {
    Text(InlineValueText),
    VariableLookup(InlineValueVariableLookup),
    EvaluatableExpression(InlineValueEvaluatableExpression),
});

impl From<InlineValueText> for InlineValue {
    #[inline]
    fn from(from: InlineValueText) -> Self {
        Self::Text(from)
    }
}

impl From<InlineValueVariableLookup> for InlineValue {
    #[inline]
    fn from(from: InlineValueVariableLookup) -> Self {
        Self::VariableLookup(from)
    }
}

impl From<InlineValueEvaluatableExpression> for InlineValue {
    #[inline]
    fn from(from: InlineValueEvaluatableExpression) -> Self {
        Self::EvaluatableExpression(from)
    }
}

/// Client workspace capabilities specific to inline values.
///
/// @since 3.17.0
#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct InlineValueWorkspaceClientCapabilities {
    pub refresh_support: Option<bool>,
}

impl_lsp_struct!(
    InlineValueWorkspaceClientCapabilities,
    required {},
    optional {
        refresh_support: bool = "refreshSupport"
    }
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;
    use crate::Position;

    #[test]
    fn inline_values() {
        test_serialization(
            &InlineValueText {
                range: Range::new(Position::new(0, 0), Position::new(0, 4)),
                text: "one".to_owned(),
            },
            r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"text":"one"}"#,
        );

        test_serialization(
            &InlineValue::VariableLookup(InlineValueVariableLookup {
                range: Range::new(Position::new(1, 0), Position::new(1, 4)),
                variable_name: None,
                case_sensitive_lookup: false,
            }),
            r#"{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":4}},"caseSensitiveLookup":false}"#,
        );

        test_serialization(
            &InlineValue::EvaluatableExpression(InlineValueEvaluatableExpression {
                range: Range::new(Position::new(2, 0), Position::new(2, 4)),
                expression: None,
            }),
            r#"{"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":4}}}"#,
        );
    }
}
