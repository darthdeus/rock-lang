//! Label patterns match exactly; a leading or trailing `*` makes a suffix, prefix or substring match.

pub fn label_matches(label: &str, pattern: &str) -> bool {
    let (lead, rest) = match pattern.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, pattern),
    };
    match (lead, rest.strip_suffix('*')) {
        (true, Some(mid)) => label.contains(mid),
        (false, Some(head)) => label.starts_with(head),
        (true, None) => label.ends_with(rest),
        (false, None) => label == rest,
    }
}

#[track_caller]
pub fn assert_offers<S: AsRef<str>>(labels: &[S], wanted: &[&str]) {
    let have: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    for pattern in wanted {
        assert!(
            have.iter().any(|l| label_matches(l, pattern)),
            "`{pattern}` not offered: {have:?}"
        );
    }
}

#[track_caller]
pub fn assert_lacks<S: AsRef<str>>(labels: &[S], unwanted: &[&str]) {
    let have: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    for pattern in unwanted {
        assert!(
            !have.iter().any(|l| label_matches(l, pattern)),
            "`{pattern}` must not be offered: {have:?}"
        );
    }
}
