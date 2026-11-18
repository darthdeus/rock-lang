[[tests]]
name = "hover_enum_variant_pattern_names_the_variant"
kind = "hover"
position = [4, 17]
source = '''
enum MobKind { Skeleton, Zombie }

fn name(k: MobKind) -> string {
    match k {
        MobKind::Skeleton => "s"
        MobKind::Zombie => "z"
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nMobKind::Skeleton\n```" }]

[[tests]]
name = "hover_enum_qualifier_in_pattern_shows_the_body"
kind = "hover"
position = [4, 8]
source = { from_test = "hover_enum_variant_pattern_names_the_variant" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nenum MobKind {\n  Skeleton,\n  Zombie,\n}\n```" }]

[[tests]]
name = "hover_double_colon_in_pattern_shows_the_enum_body"
kind = "hover"
position = [4, 15]
source = { from_test = "hover_enum_qualifier_in_pattern_shows_the_body" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nenum MobKind {\n  Skeleton,\n  Zombie,\n}\n```" }]
