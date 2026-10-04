//! The identity-storage census (docs/identity-storage-plan.md S0a-S0c) pinned
//! exactly. Exempt packages and module prefixes report no diagnostics, so this
//! table is where their debt stays visible: a migration that removes sites
//! updates the row, and an entry whose row reaches zero leaves
//! `bone_semantic::identity_storage::IDENTITY_STORAGE_EXEMPT` (or
//! `IDENTITY_STORAGE_EXEMPT_MODULES`) in the same commit. A module prefix's
//! sites are pinned one by one.

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
| anvil | exempt | 0 | 4 | 0 | 0 | 10 | 0 | 0 | 0 | 0 | 14 |
| bitgun | exempt | 14 | 8 | 0 | 0 | 68 | 1 | 1 | 70 | 1 | 163 |
| bopt | exempt | 2 | 2 | 0 | 0 | 102 | 0 | 0 | 0 | 0 | 106 |
| chess | exempt | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 19 | 0 | 21 |
| chess_game | exempt | 4 | 1 | 0 | 0 | 1 | 1 | 0 | 0 | 0 | 7 |
| chesslings | exempt | 2 | 3 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 13 |
| sample_game_c | exempt | 2 | 2 | 0 | 0 | 8 | 0 | 0 | 5 | 0 | 17 |
| sample_game_c_v0 | exempt | 1 | 2 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 11 |
| sample_game_b | exempt | 0 | 3 | 0 | 0 | 25 | 0 | 0 | 0 | 0 | 28 |
| decorator_systems | exempt | 0 | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 2 |
| idle_core | exempt | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 3 |
| logo | exempt | 1 | 1 | 0 | 0 | 8 | 0 | 0 | 0 | 0 | 10 |
| mag | exempt | 234 | 186 | 7 | 0 | 1015 | 18 | 68 | 1285 | 4 | 2817 |
| mag_ai | exempt | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 1 |
| mag_content | exempt | 0 | 3 | 2 | 0 | 8 | 0 | 0 | 0 | 0 | 13 |
| mag_encounter_worker | exempt | 3 | 1 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 6 |
| mag_simulation::nav | exempt module | 5 | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 10 |
| mag_world | exempt | 64 | 5 | 0 | 0 | 62 | 1 | 0 | 0 | 0 | 132 |
| match3 | exempt | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 15 | 6 | 21 |
| pixel_spirit | exempt | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| spark | exempt | 2 | 2 | 0 | 0 | 16 | 0 | 0 | 0 | 0 | 20 |
| std | exempt, zero: remove from the list | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| stone | exempt | 40 | 58 | 6 | 0 | 226 | 5 | 0 | 0 | 2 | 337 |
| survivors | exempt | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 4 | 4 |
| testbed | exempt | 5 | 3 | 0 | 0 | 135 | 2 | 2 | 74 | 56 | 277 |
| typography | exempt | 0 | 0 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 2 |
| unpackaged programs | exempt | 110 | 14 | 8 | 0 | 137 | 50 | 1 | 0 | 47 | 367 |
| void | exempt | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 2 |
| wgpu_renderer | exempt | 0 | 0 | 0 | 0 | 143 | 0 | 0 | 12 | 0 | 155 |

Exempt module sites:
- mag_simulation::nav: mag-simulation/nav/areas.rock:25 container mag_simulation::nav::areas::NavAreaRoute
- mag_simulation::nav: mag-simulation/nav/areas.rock:43 field mag_simulation::nav::areas::NavWorkCounters
- mag_simulation::nav: mag-simulation/nav/areas.rock:46 container mag_simulation::nav::areas::NavArea
- mag_simulation::nav: mag-simulation/nav/areas.rock:50 container mag_simulation::nav::areas::NavAreaPair
- mag_simulation::nav: mag-simulation/nav/graph.rock:285 field mag_simulation::nav::query::NavQuery
- mag_simulation::nav: mag-simulation/nav/query.rock:142 field FlatVec
- mag_simulation::nav: mag-simulation/nav/query.rock:160 container mag_simulation::nav::query::NavGoalAttachment
- mag_simulation::nav: mag-simulation/nav/query.rock:163 container mag_simulation::nav::query::NavPlan
- mag_simulation::nav: mag-simulation/nav/query.rock:169 field mag_simulation::nav::areas::NavAreas
- mag_simulation::nav: mag-simulation/nav/query.rock:210 field mag_simulation::nav::heap::NavHeap
- mag_simulation::nav: mag-simulation/nav/query.rock:609 local-storage mag_simulation::nav::query::NavGoalAttachment
- mag_simulation::nav: mag-simulation/nav/query.rock:664 local-storage mag_simulation::nav::query::NavGoalAttachment
- mag_simulation::nav: mag-simulation/nav/query.rock:669 local-storage mag_simulation::nav::query::NavGoalAttachment
- mag_simulation::nav: mag-simulation/nav/walker.rock:23 container mag_simulation::nav::walker::NavProbe

NOT fully analyzed:
- tools/mag-encounter-lab: 1 semantic errors
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
