use super::{resolve_test_shards, test_shard_count_from};

#[test]
fn default_shards_use_the_selected_count_and_program_size() {
    let mib = 1024 * 1024;
    for (tests, bytes, expected) in [
        (0, 8 * mib, 1),
        (59, 8 * mib, 1),
        (60, 8 * mib - 1, 1),
        (60, 8 * mib, 2),
        (600, 64 * mib, 2),
    ] {
        assert_eq!(
            resolve_test_shards(2, tests, bytes, false),
            expected,
            "tests={tests}, bytes={bytes}"
        );
    }
}

#[test]
fn explicit_shards_override_the_size_rule_and_core_default() {
    for value in ["1", "2", " 3 ", "8"] {
        let requested = test_shard_count_from(Some(value), 1).unwrap();
        assert_eq!(requested, value.trim().parse::<usize>().unwrap());
        assert_eq!(resolve_test_shards(requested, 5, 1024, true), requested);
    }
    for value in ["0", "-1", "bad", ""] {
        assert!(test_shard_count_from(Some(value), 32).is_err(), "{value}");
    }
}

#[test]
fn one_core_defaults_to_one_shard_for_large_packages() {
    assert_eq!(test_shard_count_from(None, 1).unwrap(), 1);
    assert_eq!(resolve_test_shards(1, 600, 64 * 1024 * 1024, false), 1);
    assert_eq!(test_shard_count_from(None, 32).unwrap(), 2);
}
