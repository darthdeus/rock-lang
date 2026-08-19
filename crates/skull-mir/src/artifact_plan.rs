//! Compiler-owned publication metadata for one runtime Artifact.
//!
//! Physical names are payload. Membership and the logical owner of a binding
//! are decided here, once, before either backend publishes an Artifact.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactBindingKind {
    Global,
    Callable,
}

impl ArtifactBindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Callable => "callable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactBinding {
    /// Compiler identity. This is never a diagnostic or backend name.
    pub identity: String,
    /// Backend-owned top-level spelling used by the Lua realization.
    pub physical: String,
    /// Source-facing spelling retained for diagnostics only.
    pub diagnostic: String,
    pub kind: ArtifactBindingKind,
    /// A mutable global is routed to this compiler-assigned Session slot.
    pub state_key: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactBindingCensus {
    pub examined: usize,
    pub assigned: usize,
    pub duplicate: usize,
    pub incomplete: usize,
}

impl ArtifactBindingCensus {
    pub fn is_total(&self) -> bool {
        self.examined > 0
            && self.examined == self.assigned
            && self.duplicate == 0
            && self.incomplete == 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactBindingPlan {
    pub entries: Vec<ArtifactBinding>,
    pub census: ArtifactBindingCensus,
}

impl ArtifactBindingPlan {
    pub fn from_entries(entries: Vec<ArtifactBinding>) -> Result<Self, String> {
        let mut census = ArtifactBindingCensus::default();
        let mut physical = BTreeMap::<String, String>::new();
        let mut ordered = Vec::with_capacity(entries.len());
        for entry in entries {
            census.examined += 1;
            if entry.identity.is_empty() || entry.physical.is_empty() {
                return Err(format!(
                    "incomplete Artifact binding `{}` `{}`",
                    entry.identity, entry.physical
                ));
            }
            if let Some(old) = physical.insert(entry.physical.clone(), entry.identity.clone()) {
                return Err(format!(
                    "Artifact binding `{}` is owned by both `{old}` and `{}`",
                    entry.physical, entry.identity
                ));
            }
            census.assigned += 1;
            ordered.push(entry);
        }
        ordered.sort_by(|a, b| {
            (&a.physical, &a.identity, a.kind).cmp(&(&b.physical, &b.identity, b.kind))
        });
        let plan = Self { entries: ordered, census };
        if !plan.entries.is_empty() && !plan.census.is_total() {
            return Err(format!("incomplete Artifact binding census: {:?}", plan.census));
        }
        Ok(plan)
    }

    pub fn entry(&self, physical: &str) -> Option<&ArtifactBinding> {
        self.entries.iter().find(|entry| entry.physical == physical)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShadowEffectPolicy {
    Pure,
    Virtualized,
    Reject,
}

impl ShadowEffectPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Virtualized => "virtualized",
            Self::Reject => "reject",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowEffect {
    /// Logical callable identity or compiler-owned effect identity.
    pub identity: String,
    pub physical: String,
    pub diagnostic: String,
    pub policy: ShadowEffectPolicy,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShadowEffectCensus {
    pub examined: usize,
    pub classified: usize,
    pub pure: usize,
    pub virtualized: usize,
    pub rejected: usize,
    pub duplicate: usize,
}

impl ShadowEffectCensus {
    pub fn is_total(&self) -> bool {
        self.examined > 0
            && self.examined == self.classified
            && self.duplicate == 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShadowEffectPlan {
    pub entries: Vec<ShadowEffect>,
    pub census: ShadowEffectCensus,
    pub admissible: bool,
}

impl ShadowEffectPlan {
    pub fn from_entries(entries: Vec<ShadowEffect>) -> Result<Self, String> {
        let mut census = ShadowEffectCensus::default();
        let mut seen = BTreeMap::<String, ShadowEffectPolicy>::new();
        let mut ordered = Vec::with_capacity(entries.len());
        for entry in entries {
            census.examined += 1;
            if entry.identity.is_empty() || entry.physical.is_empty() {
                return Err(format!(
                    "incomplete shadow effect `{}` `{}`",
                    entry.identity, entry.physical
                ));
            }
            if seen.insert(entry.physical.clone(), entry.policy).is_some() {
                return Err(format!("duplicate shadow effect `{}`", entry.physical));
            }
            census.classified += 1;
            match entry.policy {
                ShadowEffectPolicy::Pure => census.pure += 1,
                ShadowEffectPolicy::Virtualized => census.virtualized += 1,
                ShadowEffectPolicy::Reject => census.rejected += 1,
            }
            ordered.push(entry);
        }
        ordered.sort_by(|a, b| (&a.physical, &a.identity).cmp(&(&b.physical, &b.identity)));
        let plan = Self {
            admissible: census.is_total() && census.rejected == 0,
            entries: ordered,
            census,
        };
        if !plan.entries.is_empty() && !plan.census.is_total() {
            return Err(format!("incomplete shadow effect census: {:?}", plan.census));
        }
        Ok(plan)
    }

    pub fn entry(&self, physical: &str) -> Option<&ShadowEffect> {
        self.entries.iter().find(|entry| entry.physical == physical)
    }
}
