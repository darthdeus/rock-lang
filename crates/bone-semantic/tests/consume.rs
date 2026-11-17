use bone_semantic::prelude::*;

fn errors(source: &str) -> Vec<CompilerError> {
    let mut modules = [skull_parser::parse_module_clean(source, ustr("<test>")).unwrap()];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
    ws.inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten().cloned())
        .collect()
}

const TICKET: &str = "@consume struct Ticket { id: int }
fn spend(t: Ticket) -> int { return t.id }
fn next(t: Ticket) -> int { return t.id }
struct Holder { ticket: Ticket }
fn iter(xs: [Ticket]) -> Holder { return Holder { ticket: xs[0] } }
fn next(it: Holder) -> (bool, Ticket) { return (false, it.ticket) }
";

#[test]
fn rejects_consumed_bindings_with_both_use_sites() {
    for body in [
        "let t = Ticket { id: 1 }; let _ = spend(t); let _ = spend(t)",
        "let (t, _) = (Ticket { id: 1 }, 0); let _ = spend(t); let _ = spend(t)",
        "let candidate: Ticket? = Ticket { id: 1 }; let t = candidate else { return }; let _ = spend(t); let _ = spend(t)",
        "for t in [Ticket { id: 1 }] { let _ = spend(t); let _ = spend(t) }",
        "let t = Ticket { id: 1 }; let alias = t; let _ = spend(t)",
        "let t = Ticket { id: 1 }; if true { let _ = spend(t) }; let _ = spend(t)",
        "let t = Ticket { id: 1 }; if true { let _ = spend(t) } else { let _ = spend(t) }; let _ = spend(t)",
        "let t = Ticket { id: 1 }; while false { let _ = spend(t) }",
        "let t = Ticket { id: 1 }; let f = () => t.id; let _ = spend(t)",
        "let t = Ticket { id: 1 }; let _ = spend(t); let _ = next(t)",
        "let t = Ticket { id: 1 }; let _ = spend(t); let _ = t.id",
        "let f = () => { let t = Ticket { id: 1 }; let _ = spend(t); spend(t) }",
        "let t = Ticket { id: 1 }; defer spend(t); let _ = spend(t)",
        "helper(); return; fn helper() { let t = Ticket { id: 1 }; let _ = spend(t); let _ = spend(t) }",
    ] {
        let source = format!("{TICKET} fn main() {{ {body} }}");
        let errs = errors(&source);
        let err = errs
            .iter()
            .find(|err| err.message.contains("consum"))
            .unwrap_or_else(|| panic!("accepted {body}: {errs:?}"));
        assert_eq!(err.labels.len(), 2, "{body}: {err:?}");
        assert!(
            err.labels
                .iter()
                .all(|label| label.span.file == ustr("<test>"))
        );
    }
}

#[test]
fn admits_single_use_exclusive_branches_and_fresh_bindings() {
    for body in [
        "let t = Ticket { id: 1 }; let _ = spend(t)",
        "let t = Ticket { id: 1 }; if true { let _ = spend(t) } else { let _ = spend(t) }",
        "let t = Ticket { id: 1 }; let _ = spend(t); t = Ticket { id: 2 }; let _ = spend(t)",
        "let t = Ticket { id: 1 }; let _ = spend(t); let t = Ticket { id: 2 }; let _ = spend(t)",
        "while false { let t = Ticket { id: 1 }; let _ = spend(t) }",
        "for t in [Ticket { id: 1 }] { let _ = spend(t) }",
        "let (t, _) = (Ticket { id: 1 }, 0); let _ = spend(t)",
        "let t = Ticket { id: 1 }; let candidate: int? = 1; let n = candidate else { let _ = spend(t); return }; let _ = spend(t)",
        "while false { let candidate: Ticket? = Ticket { id: 1 }; let t = candidate else { break }; let _ = spend(t) }",
        "let t = Ticket { id: 1 }; let _ = next(t); let _ = t.next(); let _ = t.id; let _ = spend(t)",
        "let t = Ticket { id: 1 }; let alias = t; let _ = spend(alias)",
        "let t = Ticket { id: 1 }; let _ = next((t: Ticket)); let _ = (t: Ticket).id; let _ = spend(t)",
        "let t = Ticket { id: 1 }; if true { let _ = spend(t); return }; let _ = spend(t)",
        "let t = Ticket { id: 1 }; defer spend(t)",
        "let t = Ticket { id: 1 }; let f = () => { let _ = t.id; spend(t) }",
        "let t = Ticket { id: 1 }; let holder = Holder { ticket: t }; let _ = holder.ticket; let _ = holder.ticket",
    ] {
        let errs = errors(&format!("{TICKET} fn main() {{ {body} }}"));
        assert!(errs.is_empty(), "{body}: {errs:?}");
    }
}

#[test]
fn diagnostic_points_to_the_first_and_rejected_reads() {
    let source = format!(
        "{TICKET} fn main() {{ let t = Ticket {{ id: 1 }}; let _ = spend(t); let _ = spend(t) }}"
    );
    let errors = errors(&source);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let error = &errors[0];
    assert_eq!(error.labels.len(), 2);
    let first = source.find("spend(t)").unwrap() + "spend(".len();
    let again = source.rfind("spend(t)").unwrap() + "spend(".len();
    assert_eq!(error.labels[0].span.offset_range(), (again, again + 1));
    assert_eq!(error.labels[1].span.offset_range(), (first, first + 1));
}

#[test]
fn function_fields_project_the_receiver_and_consume_their_arguments() {
    let declarations =
        format!("{TICKET} @consume struct Action {{ run: fn() -> int, next: fn(Ticket) -> int }}");
    let source = format!(
        "{declarations} fn main() {{
            let action = Action {{ run: () => 1, next: t => t.id }}
            let _ = action.run()
            let _ = action.run()
        }}"
    );
    assert!(errors(&source).is_empty());

    let source = format!(
        "{declarations} fn main() {{
            let action = Action {{ run: () => 1, next: t => t.id }}
            let t = Ticket {{ id: 1 }}
            let _ = action.next(t)
            let _ = action.next(t)
        }}"
    );
    let errors = errors(&source);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "use of consumed binding `t`");
}

#[test]
fn iterator_bound_tracks_generic_bindings_with_both_use_sites() {
    let source = "fn drain<I: Iterator<int>>(it: I) {
        for x in it {}
        for x in it {}
    }";
    let errs = errors(source);
    assert_eq!(errs.len(), 1, "{errs:?}");
    let error = &errs[0];
    assert_eq!(error.message, "use of consumed binding `it`");
    assert_eq!(error.labels.len(), 2);
    let first = source.find("in it").unwrap() + "in ".len();
    let again = source.rfind("in it").unwrap() + "in ".len();
    assert_eq!(error.labels[0].span.offset_range(), (again, again + 2));
    assert_eq!(error.labels[1].span.offset_range(), (first, first + 2));
}

#[test]
fn iterator_bound_uses_the_concrete_consumption_rules() {
    for body in [
        "for x in it {}; let _ = next(it)",
        "let alias = it; for x in it {}",
        "if true { for x in it {} }; for x in it {}",
        "while false { for x in it {} }",
        "let read = () => next(it); for x in it {}",
    ] {
        let errs = errors(&format!("fn drain<I: Iterator<int>>(it: I) {{ {body} }}"));
        assert_eq!(errs.len(), 1, "{body}: {errs:?}");
        assert!(errs[0].message.contains("consum"), "{body}: {errs:?}");
        assert_eq!(errs[0].labels.len(), 2, "{body}: {errs:?}");
    }
    for body in [
        "let _ = next(it); let _ = it.next(); for x in it {}",
        "for x in it {}; it = fresh; let _ = next(it); for x in it {}",
        "if true { for x in it {} } else { for x in it {} }",
        "let alias = it; for x in alias {}",
    ] {
        let errs = errors(&format!(
            "fn drain<I: Iterator<int>>(it: I, fresh: I) {{ {body} }}"
        ));
        assert!(errs.is_empty(), "{body}: {errs:?}");
    }
}

#[test]
fn iterator_bound_does_not_track_other_generic_parameters() {
    let errs = errors(
        "fn repeat<T>(value: T) { let a = value; let b = value }
         fn drain<T, I: Iterator<T>>(it: I, value: T) {
             let a = value
             let b = value
             for x in it {}
         }",
    );
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn every_iterator_bound_is_tracked() {
    let errs = errors(
        "fn drain<T, I: Iterator<T>, J: Iterator<int>>(first: I, second: J) {
             for x in first {}
             for x in first {}
             for x in second {}
             for x in second {}
         }",
    );
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert_eq!(errs[0].message, "use of consumed binding `first`");
    assert_eq!(errs[1].message, "use of consumed binding `second`");
    assert!(errs.iter().all(|err| err.labels.len() == 2));
}
