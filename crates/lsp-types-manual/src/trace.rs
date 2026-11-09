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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_serialization;

    #[test]
    fn test_trace_value() {
        test_serialization(
            &vec![TraceValue::Off, TraceValue::Messages, TraceValue::Verbose],
            r#"["off","messages","verbose"]"#,
        );
    }
}
