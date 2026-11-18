[[tests]]
name = "named_arg_completion_at_call_site"
kind = "completion"
position = [5, 20]
source = '''
fn greet(greeting: string, name: string) -> string {
    return greeting
}

fn main() {
    let x = greet(gr)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["greeting:"] }]

[[tests]]
name = "named_arg_completion_empty_prefix"
kind = "completion"
position = [4, 14]
source = '''
fn configure(width: int, height: int, title: string) {
}

fn main() {
    configure()
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["width:", "height:", "title:"] }]

[[tests]]
name = "named_arg_completion_skips_earlier_named"
kind = "completion"
position = [4, 25]
source = { from_test = "named_arg_completion_empty_prefix", replace = [["    configure()", "    configure(width: 10, )"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["height:", "title:"] }]

[[tests]]
name = "named_arg_completion_skips_positional"
kind = "completion"
position = [4, 18]
source = { from_test = "named_arg_completion_empty_prefix", replace = [["    configure()", "    configure(10, )"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["height:", "title:"] }]
