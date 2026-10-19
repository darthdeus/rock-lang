//! The host package graph's dependency-alias tables, in bone's shape.
//!
//! A v1 fragment resolves a dependency under the alias its own manifest
//! declared, so the resolver needs the graph's `alias -> package id` edges
//! before any file is lowered. Both entry points into bone's front end need
//! the same table from the same graph: the batch compile
//! (`semantic_pass_with_aliases`) and the incremental `Session` behind
//! CLI operations and the LSP. This is the one place that maps one to
//! the other.

use std::path::Path;

use bone_core::modules::{NsAliases, PackageAliases};

/// Build the alias tables for the package rooted at `manifest_dir`.
pub fn for_manifest_dir(manifest_dir: &Path) -> Result<NsAliases, String> {
    let graph = rock_host::stick::graph::PackageGraph::build(manifest_dir)
        .map_err(|error| format!("cannot build package graph for modules-v1 aliases: {error}"))?;
    Ok(from_graph(&graph))
}

/// [`for_manifest_dir`] for a graph the caller already built.
pub fn from_graph(graph: &rock_host::stick::graph::PackageGraph) -> NsAliases {
    NsAliases {
        packages: graph
            .packages()
            .iter()
            .map(|node| PackageAliases {
                id: node.id.0,
                self_name: ustr::Ustr::from(node.self_name.as_str()),
                aliases: node
                    .dependencies
                    .iter()
                    .map(|edge| (ustr::Ustr::from(edge.alias.as_str()), edge.target.0))
                    .collect(),
            })
            .collect(),
    }
}
