#[test]
#[cfg(unix)]
fn run() {
    use lsp_types_manual::lsif::Entry;

    let jsonl = include_str!("tsc-unix.lsif");
    for json in jsonl.lines() {
        let r = serde_json::from_str::<Entry>(json)
            .unwrap_or_else(|error| panic!("can not parse {}: {:?}", json, error));
        let x = serde_json::to_string(&r)
            .unwrap_or_else(|error| panic!("can not serialize {}: {:?}", json, error));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&x).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap(),
            "and strings:\ntheir: {}\n  our: {}",
            json,
            x,
        );
    }
}
