use super::*;

#[test]
fn semantic_tokens_cover_valid_and_incomplete_code() {
    let cases = [
        (
            "structs, functions and locals",
            "struct Point {\n    x: float,\n    y: float,\n}\n\nfn distance(p1: Point, p2: Point) -> float {\n    let dx = p2.x - p1.x\n    let dy = p2.y - p1.y\n    return (dx * dx + dy * dy).sqrt()\n}\n\nfn main() {\n    let origin = Point { x: 0.0, y: 0.0 }\n    print(distance(origin, Point { x: 3.0, y: 4.0 }).to_string())\n}",
            30,
        ),
        (
            "undefined names and a half-typed call",
            "struct Sprite {\n    width: int,\n}\n\nfn draw_sprite(sprite: Sprite, pos: Vec2) {\n}\n\nfn update_entities() {\n    let sprite_frog = Sprite { width: 16 }\n    let pos = Vec2 { x: 100.0, y: 200.0 }\n    for i in range(0, 10) {\n        draw_sprite(sprite_frog, pos)\n        draw_\n    }\n    let another_sprite = Sprite { width: 32 }\n    draw_sprite(another_sprite, pos)\n}",
            20,
        ),
        (
            "await delegation",
            "co fn inner() -> Coroutine<int> {\n    yield 1\n}\n\nco fn outer() -> Coroutine<int> {\n    await inner()\n    yield 3\n}",
            14,
        ),
    ];
    for (case, source, minimum) in cases {
        let reply = Project::single(source).cli("semantic-tokens", None, &[]);
        assert!(
            reply["count"].as_u64().unwrap() >= minimum,
            "{case}: {reply}"
        );
    }
}
