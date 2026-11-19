use std::fs;
use std::path::{Path, PathBuf};

mod comment_multiset;
mod corpus;
mod roundtrip;

fn collect_files(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir)
        .expect("read a fixture directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, extension, out);
        } else if path.extension().is_some_and(|ext| ext == extension) {
            out.push(path);
        }
    }
}
