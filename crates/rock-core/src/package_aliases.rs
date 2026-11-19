//! Package dependency-alias facts shared by the host and bone's resolver.
//!

use ustr::Ustr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsAliases {
    pub packages: Vec<PackageAliases>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageAliases {
    pub id: u32,
    pub self_name: Ustr,
    pub aliases: Vec<(Ustr, u32)>,
}
