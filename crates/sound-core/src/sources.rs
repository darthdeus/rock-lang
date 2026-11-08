use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::AssetResolver;

fn is_pattern(key: &str) -> bool {
    key.contains(['*', '?', '[', '{'])
}

/// Discovery is confined to one named directory; it never walks an asset store.
pub fn source_directory(key: &str) -> Result<&Path, String> {
    crate::validate_asset_key(key)?;
    let directory = Path::new(key)
        .parent()
        .ok_or_else(|| format!("source has no directory: {key}"))?;
    if is_pattern(&directory.to_string_lossy()) {
        return Err(format!("sound patterns must name a fixed directory: {key}"));
    }
    Ok(directory)
}

pub fn source_matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    source_directory(pattern)?;
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| format!("invalid sound pattern {pattern}: {error}"))
}

pub(crate) fn discover_files(roots: &[PathBuf], pattern: &str) -> Result<Vec<String>, String> {
    let directory = source_directory(pattern)?;
    let matcher = source_matcher(pattern)?;
    let mut keys = BTreeSet::new();
    for root in roots {
        let path = root.join(directory);
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read sound directory {}: {error}", path.display())),
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("read sound directory {}: {error}", path.display()))?;
            let key = directory.join(entry.file_name());
            if !matcher.is_match(&key) {
                continue;
            }
            let metadata = entry.path().metadata().map_err(|error| {
                format!("read sound source {}: {error}", entry.path().display())
            })?;
            if !metadata.is_file() {
                continue;
            }
            let key = key
                .to_str()
                .ok_or_else(|| format!("sound source is not UTF-8: {}", key.display()))?;
            keys.insert(key.replace('\\', "/"));
        }
    }
    Ok(keys.into_iter().collect())
}

pub fn expand_sources<R: AssetResolver + ?Sized>(
    sources: &[String],
    resolver: &R,
) -> Result<Vec<String>, String> {
    let mut expanded = Vec::new();
    let discovered = sources.iter().any(|key| is_pattern(key));
    for key in sources {
        crate::validate_asset_key(key)?;
        if is_pattern(key) {
            let mut matches = resolver.discover(key)?;
            matches.sort();
            matches.dedup();
            if matches.is_empty() {
                return Err(format!("sound pattern matches no files: {key}"));
            }
            expanded.extend(matches);
        } else {
            expanded.push(key.clone());
        }
    }
    if discovered {
        let mut seen = BTreeSet::new();
        for key in &expanded {
            if !seen.insert(key) {
                return Err(format!("overlapping sound selections repeat {key}"));
            }
        }
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsAssetResolver;

    #[test]
    fn discovery_unions_roots_but_resolution_keeps_overlay_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("overlay");
        let project = tmp.path().join("project");
        for root in [&overlay, &project] {
            std::fs::create_dir_all(root.join("assets/steps")).unwrap();
        }
        for (root, name) in [
            (&overlay, "step_02.wav"),
            (&project, "step_01.wav"),
            (&project, "step_02.wav"),
            (&overlay, "preview.wav"),
        ] {
            std::fs::write(root.join("assets/steps").join(name), b"fixture").unwrap();
        }
        let resolver = FsAssetResolver::new(vec![overlay.clone(), project]);
        let selection = vec!["assets/steps/step_*.wav".into()];
        assert_eq!(
            expand_sources(&selection, &resolver).unwrap(),
            ["assets/steps/step_01.wav", "assets/steps/step_02.wav"]
        );
        assert_eq!(
            resolver.resolve("assets/steps/step_02.wav").unwrap(),
            overlay.join("assets/steps/step_02.wav")
        );
        std::fs::write(overlay.join("assets/steps/step_03.wav"), b"new").unwrap();
        assert_eq!(expand_sources(&selection, &resolver).unwrap().len(), 3);
        std::fs::remove_file(overlay.join("assets/steps/step_03.wav")).unwrap();
        assert_eq!(expand_sources(&selection, &resolver).unwrap().len(), 2);
    }

    #[test]
    fn empty_overlapping_and_unbounded_selections_fail() {
        let resolver = crate::AssetMapResolver::new(std::collections::BTreeMap::from([(
            "assets/steps/step_01.wav".into(),
            PathBuf::from("unused"),
        )]));
        for (selection, message) in [
            (vec!["assets/steps/absent_*.wav"], "matches no files"),
            (
                vec!["assets/steps/*.wav", "assets/steps/step_01.wav"],
                "overlapping",
            ),
            (vec!["assets/*/step_01.wav"], "fixed directory"),
            (vec!["assets/steps/[.wav"], "invalid sound pattern"),
        ] {
            let selection = selection.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let error = expand_sources(&selection, &resolver).unwrap_err();
            assert!(error.contains(message), "{error}");
        }
    }
}
