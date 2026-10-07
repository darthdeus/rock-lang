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
    };
    for _ in 0..10_000 {
        ctx.probe_legacy(ustr("a"));
        ctx.probe_legacy(ustr("b"));
    }
    ctx.flush_scope_probes();
    ctx.probe_legacy(ustr("b"));
    ctx.probe_legacy(ustr("c"));
    ctx.flush_scope_probes();

    let probes = &module.scope_probes;
    let expected = ["a", "b", "c"].map(|name| ScopeProbe::Legacy { name: ustr(name) });
    assert_eq!(probes.len(), expected.len());
    assert!(expected.iter().all(|p| probes.contains(p)));
    assert!(
        probes.capacity() <= 4 * probes.len().max(16),
        "{} distinct probes reserved capacity {}",
        probes.len(),
        probes.capacity()
    );
}
