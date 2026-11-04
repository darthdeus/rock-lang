use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bone_core::modules::{NsNode, NsPackage, ScopeProbe};
use bone_core::workspace::Workspace;
use bone_lower::resolve::{module_members, with_probes};
use ustr::Ustr;

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn workspace() -> (Workspace, Vec<Ustr>) {
    let mut ws = Workspace::new();
    let path = vec![Ustr::from("outer"), Ustr::from("inner")];
    let mut pkg = NsPackage {
        id: 1,
        ..Default::default()
    };
    for len in 0..=path.len() {
        let key = NsPackage::path_key(&path[..len]);
        let mut node = NsNode {
            path_key: key,
            segments: path[..len].to_vec(),
            ..Default::default()
        };
        if len < path.len() {
            node.children
                .insert(path[len], NsPackage::path_key(&path[..=len]));
        }
        pkg.nodes.insert(key, node);
    }
    ws.ns.packages.insert(1, pkg);
    (ws, path)
}

#[test]
fn repeated_namespace_members_do_not_construct_key_strings() {
    let (ws, path) = workspace();
    let name = Ustr::from("absent");
    module_members(&ws, &[], 1, &path, name).unwrap();
    ALLOCATIONS.with(|count| count.set(Some(0)));
    for _ in 0..64 {
        let members = module_members(&ws, &[], 1, &path, name).unwrap();
        assert!(members.found.is_empty());
        assert!(!members.denied && !members.cycle && !members.module);
    }
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    assert_eq!(allocations, 0, "existing namespace keys must be reused");
}

#[test]
fn namespace_probes_keep_existing_and_missing_path_keys() {
    let (ws, mut path) = workspace();
    let name = Ustr::from("absent");
    for package in [1, 2] {
        for _ in 0..2 {
            let key = NsPackage::path_key(&path);
            let (_, probes) =
                with_probes(|| module_members(&ws, &[], package, &path, name).unwrap());
            assert_eq!(
                probes,
                vec![
                    ScopeProbe::Member {
                        package,
                        node: key,
                        name
                    };
                    2
                ]
            );
            path.push(Ustr::from("missing"));
        }
    }
}
