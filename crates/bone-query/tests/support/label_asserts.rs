//! Assertions over completion label lists, shared by the completion tests.

#[track_caller]
pub fn assert_offers<S: AsRef<str>>(labels: &[S], wanted: &[&str]) {
    let have: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    for label in wanted {
        assert!(have.contains(label), "`{label}` not offered: {have:?}");
    }
}

#[track_caller]
pub fn assert_lacks<S: AsRef<str>>(labels: &[S], unwanted: &[&str]) {
    let have: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    for label in unwanted {
        assert!(
            !have.contains(label),
            "`{label}` must not be offered: {have:?}"
        );
    }
}
