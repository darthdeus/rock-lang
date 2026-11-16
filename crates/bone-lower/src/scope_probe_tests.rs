use bone_core::{hir, modules::ScopeProbe, workspace::Workspace};
use ustr::ustr;

use crate::LowerCtx;

/// A module's lookups repeat the same probes many times; the module's probe
/// set must be sized by its distinct probes, not by the recorded count.
#[test]
fn repeated_probes_do_not_inflate_the_module_probe_set() {
    let ws = Workspace::new();
    let mut module = hir::Module::default();
    let mut ctx = LowerCtx {
        workspace: &ws,
        module: &mut module,
        generics_in_scope: Default::default(),
        _next_var_id: 0,
        placeholder_param: None,
        placeholder_forbidden: false,
        scope_probes: Default::default(),
        resolutions: Default::default(),
        place_temps: Vec::new(),
        name_errors: Default::default(),
    };
    let node = |name| ScopeProbe::Node {
        package: 1,
        node: ustr(name),
    };
    let probe = |ctx: &LowerCtx, name| ctx.scope_probes.borrow_mut().push(node(name));
    for _ in 0..10_000 {
        probe(&ctx, "a");
        probe(&ctx, "b");
    }
    ctx.flush_scope_probes();
    probe(&ctx, "b");
    probe(&ctx, "c");
    ctx.flush_scope_probes();

    let probes = &module.scope_probes;
    let expected = ["a", "b", "c"].map(node);
    assert_eq!(probes.len(), expected.len());
    assert!(expected.iter().all(|p| probes.contains(p)));
    assert!(
        probes.capacity() <= 4 * probes.len().max(16),
        "{} distinct probes reserved capacity {}",
        probes.len(),
        probes.capacity()
    );
}
