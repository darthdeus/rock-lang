//! Package dependency-alias facts shared by the host and bone's resolver.
//!
//! A v1 fragment resolves a dependency under the alias its own manifest
//! declared, so the resolver needs every package's `alias -> package id`
//! edges before any file is lowered. The host package graph (`rock-host`)
//! produces these tables; bone's namespace resolver consumes them. They live
//! here so neither side depends on the other.

use ustr::Ustr;

/// Alias tables, one entry per package in the graph. Tests construct this
/// directly; builds derive it from the host package graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsAliases {
    pub packages: Vec<PackageAliases>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageAliases {
    /// Dense package id from the graph build.
    pub id: u32,
    pub self_name: Ustr,
    /// alias -> target package id.
    pub aliases: Vec<(Ustr, u32)>,
}
