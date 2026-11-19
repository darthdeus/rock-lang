use super::*;

#[test]
fn named_arg_completion_at_call_site() {
    let source = r#"fn greet(greeting: string, name: string) -> string {
    return greeting
}

fn main() {
    let x = greet(gr)
}"#;
    let project = Project::single("named_arg_completion_at_call_site", source);
    let reply = project.cli("completion", Some((5, 20)), &[]);
    completion_labels(&reply, &["greeting:"]);
}

const NAMED_ARG_COMPLETION_EMPTY_PREFIX: &str = r#"fn configure(width: int, height: int, title: string) {
}

fn main() {
    configure()
}"#;

#[test]
fn named_arg_completion_empty_prefix() {
    let source = NAMED_ARG_COMPLETION_EMPTY_PREFIX;
    let project = Project::single("named_arg_completion_empty_prefix", source);
    let reply = project.cli("completion", Some((4, 14)), &[]);
    completion_labels(&reply, &["width:", "height:", "title:"]);
}

#[test]
fn named_arg_completion_skips_earlier_named() {
    let source =
        &NAMED_ARG_COMPLETION_EMPTY_PREFIX.replace("    configure()", "    configure(width: 10, )");
    let project = Project::single("named_arg_completion_skips_earlier_named", source);
    let reply = project.cli("completion", Some((4, 25)), &[]);
    completion_labels(&reply, &["height:", "title:"]);
}

#[test]
fn named_arg_completion_skips_positional() {
    let source =
        &NAMED_ARG_COMPLETION_EMPTY_PREFIX.replace("    configure()", "    configure(10, )");
    let project = Project::single("named_arg_completion_skips_positional", source);
    let reply = project.cli("completion", Some((4, 18)), &[]);
    completion_labels(&reply, &["height:", "title:"]);
}
