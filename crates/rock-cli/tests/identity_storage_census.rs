//! The identity-storage census (docs/identity-storage-plan.md S0a-S0c) pinned
//! exactly. Exempt packages report no diagnostics, so this table is where
//! their debt stays visible: a migration that removes sites updates the row,
//! and a package whose row reaches zero leaves
//! `bone_semantic::identity_storage::IDENTITY_STORAGE_EXEMPT` in the same
//! commit.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

const PINNED: &str = "\
| package | status | field | container | payload | newtype | local-storage | capture | co-param | across-yield | any-erasure | total |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| anvil | exempt | 0 | 4 | 0 | 0 | 11 | 0 | 0 | 0 | 0 | 15 |
| bitgun | exempt | 20 | 8 | 0 | 0 | 72 | 1 | 1 | 95 | 2 | 199 |
| bopt | exempt | 3 | 12 | 1 | 0 | 198 | 0 | 0 | 0 | 0 | 214 |
| chess | exempt | 1 | 22 | 0 | 0 | 54 | 0 | 3 | 19 | 0 | 99 |
| chess_core | exempt | 0 | 0 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 8 |
| chess_game | exempt | 14 | 3 | 0 | 0 | 143 | 1 | 0 | 0 | 0 | 161 |
| chess_sfx | exempt | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| chesslings | exempt | 2 | 6 | 0 | 0 | 51 | 0 | 0 | 0 | 0 | 59 |
| sample_game_c | exempt | 2 | 4 | 0 | 0 | 18 | 0 | 0 | 17 | 0 | 41 |
| sample_game_c_v0 | exempt | 1 | 2 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 11 |
| sample_game_b | exempt | 1 | 11 | 0 | 0 | 150 | 0 | 0 | 49 | 0 | 211 |
| decorator_systems | exempt | 0 | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 2 |
| fm | exempt | 0 | 0 | 0 | 0 | 13 | 0 | 0 | 0 | 0 | 13 |
| fm_core | exempt | 0 | 4 | 0 | 0 | 21 | 0 | 0 | 0 | 0 | 25 |
| sample-game | exempt | 1 | 14 | 0 | 0 | 285 | 0 | 0 | 14 | 0 | 314 |
| idle_core | exempt | 4 | 18 | 0 | 0 | 57 | 0 | 0 | 0 | 0 | 79 |
| idle_reference | exempt | 0 | 0 | 0 | 0 | 94 | 0 | 0 | 0 | 0 | 94 |
| logo | exempt | 1 | 1 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 10 |
| mag | exempt | 411 | 228 | 9 | 0 | 1689 | 26 | 116 | 10466 | 63 | 13008 |
| mag_ai | exempt | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 1 |
| mag_content | exempt | 5 | 11 | 6 | 0 | 18 | 0 | 0 | 0 | 1 | 41 |
| mag_encounter_worker | exempt | 3 | 1 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 6 |
| mag_simulation | exempt | 20 | 9 | 4 | 0 | 127 | 0 | 1 | 0 | 0 | 161 |
| mag_world | exempt | 11 | 0 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 13 |
| match3 | exempt | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 15 | 7 | 23 |
| match3_core | exempt | 1 | 2 | 0 | 0 | 4 | 0 | 0 | 0 | 0 | 7 |
| pixel_spirit | exempt | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| spark | exempt | 2 | 4 | 0 | 0 | 24 | 0 | 0 | 0 | 0 | 30 |
| std | exempt | 9 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 17 | 26 |
| stone | exempt | 134 | 116 | 10 | 0 | 319 | 5 | 0 | 0 | 13 | 597 |
| stone_core | exempt | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| survivors | exempt | 0 | 3 | 0 | 0 | 1 | 0 | 0 | 0 | 5 | 9 |
| testbed | exempt | 11 | 5 | 0 | 0 | 140 | 2 | 2 | 76 | 74 | 310 |
| typography | exempt | 0 | 0 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 2 |
| ui_lab | exempt | 0 | 3 | 0 | 0 | 0 | 0 | 0 | 3 | 0 | 6 |
| ui_shader_probe | exempt | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 |
| unpackaged programs | exempt | 105 | 13 | 5 | 0 | 110 | 56 | 4 | 0 | 53 | 346 |
| vfxlab | exempt | 0 | 3 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 4 |
| void | exempt | 0 | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 2 | 4 |
| wgpu_renderer | exempt | 0 | 0 | 0 | 0 | 144 | 0 | 0 | 37 | 7 | 188 |

NOT fully analyzed:
- tests/newtype_generic_unification_reject_map_conflict.rock: 2 other semantic errors
- tests/newtype_generic_unification_reject_wrong_shape.rock: 1 other semantic errors
- tests/test_utils.rock: Cannot run test-util files directly. Test utilities are meant to be used by test files.
";

#[test]
fn identity_storage_census_is_pinned_per_package_and_kind() {
    let root = repo_root();
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(&root)
        .env("ROCK_DIR", &root)
        .args(["identity-storage-census", "."])
        .output()
        .expect("run rock identity-storage-census");
    assert!(
        output.status.success(),
        "census failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let table = String::from_utf8(output.stdout).expect("census output is UTF-8");
    assert_eq!(table, PINNED, "identity-storage census changed; update the pinned table");
}
