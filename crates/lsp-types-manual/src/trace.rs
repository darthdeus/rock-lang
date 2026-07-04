
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SetTraceParams {
    /// The new value that should be assigned to the trace setting.
    pub value: TraceValue,
}

impl_lsp_struct!(SetTraceParams,
    required { value: TraceValue = "value" },
    optional {}
);

/// A TraceValue represents the level of verbosity with which the server systematically
/// reports its execution trace using `$/logTrace` notifications.
///
/// The initial trace value is set by the client at initialization and can be modified
/// later using the `SetTrace` notification.
#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
pub enum TraceValue {
    /// The server should not send any `$/logTrace` notification
    #[default]
    Off,
    /// The server should not add the 'verbose' field in the `LogTraceParams`
    Messages,
    Verbose,
}

impl_str_enum!(TraceValue { Off = "off", Messages = "messages", Verbose = "verbose" });

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LogTraceParams {
    /// The message to be logged.
    pub message: String,
    /// Additional information that can be computed if the `trace` configuration
    /// is set to `'verbose'`
    pub verbose: Option<String>,
}

impl_lsp_struct!(LogTraceParams,
    required { message: String = "message" },
    optional { verbose: String = "verbose" }
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;

    #[test]
    fn test_set_trace_params() {
        test_serialization(
            &SetTraceParams { value: TraceValue::Off },
            r#"{"value":"off"}"#,
        );
    }

    #[test]
    fn test_log_trace_params() {
        test_serialization(
            &LogTraceParams { message: "message".into(), verbose: None },
            r#"{"message":"message"}"#,
        );
        test_serialization(
            &LogTraceParams { message: "message".into(), verbose: Some("verbose".into()) },
            r#"{"message":"message","verbose":"verbose"}"#,
        );
    }

    #[test]
    fn test_trace_value() {
        test_serialization(
            &vec![TraceValue::Off, TraceValue::Messages, TraceValue::Verbose],
            r#"["off","messages","verbose"]"#,
        );
    }
}
