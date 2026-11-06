TEST: hover_enum_variant_pattern_names_the_variant
TYPE: hover
---
enum MobKind { Skeleton, Zombie }

fn name(k: MobKind) -> string {
    match k {
        MobKind::«cursor»Skeleton => "s"
        MobKind::Zombie => "z"
    }
}
---
OPERATION: hover
---
EXPECTED:
```rock
MobKind::Skeleton
```
===

TEST: hover_enum_qualifier_in_pattern_shows_the_body
TYPE: hover
---
enum MobKind { Skeleton, Zombie }

fn name(k: MobKind) -> string {
    match k {
        «cursor»MobKind::Skeleton => "s"
        MobKind::Zombie => "z"
    }
}
---
OPERATION: hover
---
EXPECTED:
```rock
enum MobKind {
  Skeleton,
  Zombie,
}
```
===

TEST: hover_double_colon_in_pattern_shows_the_enum_body
TYPE: hover
---
enum MobKind { Skeleton, Zombie }

fn name(k: MobKind) -> string {
    match k {
        MobKind«cursor»::Skeleton => "s"
        MobKind::Zombie => "z"
    }
}
---
OPERATION: hover
---
EXPECTED:
```rock
enum MobKind {
  Skeleton,
  Zombie,
}
```
===
