//! The identity-storage census (docs/identity-storage-plan.md S0a) pinned
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
| package | status | field | container | payload | newtype | global | total |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| anvil | exempt | 0 | 4 | 0 | 0 | 0 | 4 |
| bitgun | exempt | 23 | 8 | 0 | 0 | 4 | 35 |
| bopt | exempt | 3 | 12 | 1 | 0 | 0 | 16 |
| chess | exempt | 1 | 22 | 0 | 0 | 1 | 24 |
| chess_game | exempt | 14 | 3 | 0 | 0 | 0 | 17 |
| chesslings | exempt | 2 | 6 | 0 | 0 | 0 | 8 |
| sample_game_c | exempt | 2 | 4 | 0 | 0 | 2 | 8 |
| sample_game_c_v0 | exempt | 1 | 2 | 0 | 0 | 1 | 4 |
| sample_game_b | exempt | 1 | 11 | 0 | 0 | 0 | 12 |
| decorator_systems | exempt | 0 | 1 | 0 | 0 | 0 | 1 |
| fm_core | exempt | 0 | 4 | 0 | 0 | 2 | 6 |
| sample-game | exempt | 1 | 14 | 0 | 0 | 2 | 17 |
| idle_core | exempt | 4 | 18 | 0 | 0 | 1 | 23 |
| logo | exempt | 3 | 1 | 0 | 0 | 2 | 6 |
| mag | exempt | 514 | 228 | 9 | 0 | 46 | 797 |
| mag_content | exempt | 5 | 11 | 6 | 0 | 0 | 22 |
| mag_encounter_worker | exempt | 3 | 1 | 0 | 0 | 0 | 4 |
| mag_simulation | exempt | 20 | 9 | 4 | 0 | 0 | 33 |
| mag_world | exempt | 11 | 0 | 0 | 0 | 0 | 11 |
| match3 | exempt | 0 | 1 | 0 | 0 | 1 | 2 |
| match3_core | exempt | 1 | 2 | 0 | 0 | 0 | 3 |
| spark | exempt | 2 | 4 | 0 | 0 | 0 | 6 |
| std | exempt | 11 | 1 | 0 | 0 | 0 | 12 |
| stone | exempt | 157 | 118 | 10 | 0 | 24 | 309 |
| stone_core | exempt | 1 | 0 | 1 | 0 | 0 | 2 |
| survivors | exempt | 0 | 3 | 0 | 0 | 0 | 3 |
| testbed | exempt | 13 | 5 | 0 | 0 | 26 | 44 |
| typography | exempt | 0 | 0 | 0 | 0 | 1 | 1 |
| ui_lab | exempt | 0 | 3 | 0 | 0 | 0 | 3 |
| unpackaged programs | exempt | 105 | 13 | 5 | 0 | 0 | 123 |
| vfxlab | exempt | 0 | 3 | 0 | 0 | 1 | 4 |
| void | exempt | 0 | 1 | 0 | 0 | 1 | 2 |
| wgpu_renderer | exempt | 1 | 0 | 0 | 0 | 24 | 25 |

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
