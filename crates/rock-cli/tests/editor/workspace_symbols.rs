use super::*;

#[test]
fn workspace_symbols_match_queries() {
    let source = r#"struct UserAccount { id: int }
enum UserStatus { Active, Banned }
struct Handler { name: string }
fn handle_request(user: UserAccount) {}
fn calculate_sum(a: int, b: int) -> int { a + b }
fn calculate_product(a: int, b: int) -> int { a * b }
fn POINT_DISTANCE() -> int { 0 }"#;
    let project = Project::single(source);
    let cases: &[(&str, &[(&str, u32)])] = &[
        (
            "calculate",
            &[("calculate_sum", 12), ("calculate_product", 12)],
        ),
        ("User", &[("UserAccount", 23), ("UserStatus", 10)]),
        ("hand", &[("handle_request", 12), ("Handler", 23)]),
        ("point", &[("POINT_DISTANCE", 12)]),
        (
            "",
            &[
                ("UserAccount", 23),
                ("UserStatus", 10),
                ("calculate_sum", 12),
            ],
        ),
    ];
    for (query, expected) in cases {
        let reply = project.cli("workspace-symbols", None, &[query]);
        let expected: Vec<_> = expected
            .iter()
            .map(|(name, kind)| json!({"name": name, "kind": kind}))
            .collect();
        contains_items(&reply["symbols"], &expected);
    }
}
