//! The backend-neutral ownership plan shared by boot and world images.

use std::collections::BTreeSet;

use crate::state_schema::{StateKind, StateSchema};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImageEntryKind {
    SemanticSession,
    ProgramArtifact,
    HostProjection,
    RunnerDiagnostic,
    Unsupported,
}

impl ImageEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SemanticSession => "semantic_session",
            Self::ProgramArtifact => "program_artifact",
            Self::HostProjection => "host_projection",
            Self::RunnerDiagnostic => "runner_diagnostic",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateImageEntry {
    pub identity: String,
    pub kind: ImageEntryKind,
    pub layout: String,
    pub capture_policy: String,
    pub realization_policy: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateImagePlanCensus {
    pub examined: usize,
    pub assigned: usize,
    pub duplicates: usize,
    pub unknown: usize,
    pub semantic_session: usize,
    pub program_artifact: usize,
    pub host_projection: usize,
    pub runner_diagnostic: usize,
    pub unsupported: usize,
}

impl StateImagePlanCensus {
    pub fn is_total(&self) -> bool {
        self.examined == self.assigned && self.duplicates == 0 && self.unknown == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateImagePlan {
    schema_id: String,
    entries: Vec<StateImageEntry>,
    census: StateImagePlanCensus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateImagePlanError {
    EmptySchema,
    DuplicateIdentity(String),
}

impl std::fmt::Display for StateImagePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySchema => write!(f, "StateImagePlan requires a non-empty StateSchema"),
            Self::DuplicateIdentity(identity) => {
                write!(f, "StateImagePlan owns `{identity}` more than once")
            }
        }
    }
}

impl std::error::Error for StateImagePlanError {}

impl StateImagePlan {
    /// Build the one image ownership list from compiler-described entries and
    /// the structural roots that are not HIR declarations.
    pub fn from_schema(schema: &StateSchema) -> Result<Self, StateImagePlanError> {
        if schema.entries.is_empty() {
            return Err(StateImagePlanError::EmptySchema);
        }
        let mut entries = Vec::new();
        for entry in &schema.entries {
            let (kind, capture_policy, realization_policy) = if entry.kind == StateKind::Global
                && runner_global(entry)
            {
                (ImageEntryKind::RunnerDiagnostic, "exclude", "runner-owned")
            } else if entry.kind == StateKind::Global && host_global(entry) {
                (
                    ImageEntryKind::HostProjection,
                    "host-owned",
                    "rebuild-host-projection",
                )
            } else {
                match entry.kind {
                    StateKind::Global | StateKind::Static => {
                        (ImageEntryKind::SemanticSession, "graph", "fresh-session")
                    }
                    StateKind::CoroutineFrame => (
                        ImageEntryKind::SemanticSession,
                        "frame-descriptor",
                        "fresh-session",
                    ),
                    StateKind::Storage => (
                        ImageEntryKind::SemanticSession,
                        "storage-descriptor",
                        "fresh-allocation",
                    ),
                }
            };
            entries.push(StateImageEntry {
                identity: entry.identity.clone(),
                kind,
                layout: entry.fingerprint.clone(),
                capture_policy: capture_policy.to_string(),
                realization_policy: realization_policy.to_string(),
            });
        }

        // These are structural Session fields, not a second compiler registry.
        // Their names are stable image identities; their values are realized by
        // the runtime Session rather than by a backend global.
        push_structural(
            &mut entries,
            ImageEntryKind::SemanticSession,
            "session::schema-id",
            "schema-id",
            "descriptor",
            "fresh-session",
        );
        push_structural(
            &mut entries,
            ImageEntryKind::SemanticSession,
            "session::initialized",
            "initialized-map",
            "graph",
            "fresh-session",
        );
        push_structural(
            &mut entries,
            ImageEntryKind::SemanticSession,
            "session::initializer-keys",
            "initializer-key-map",
            "graph",
            "fresh-session",
        );

        for (identity, layout, capture) in [
            ("program-artifact::code-slots", "logical-code-identities", "reference"),
            ("program-artifact::lifecycle-plan", "lifecycle-plan", "reference"),
            ("program-artifact::definition-plan", "definition-plan", "reference"),
        ] {
            push_structural(
                &mut entries,
                ImageEntryKind::ProgramArtifact,
                identity,
                layout,
                capture,
                "stable-artifact",
            );
        }

        for (identity, layout, capture) in [
            ("host-projection::arena-registry", "arena-registry", "reachable-or-full"),
            ("host-projection::native-ledger", "owned-native-resources", "rebuild"),
        ] {
            push_structural(
                &mut entries,
                ImageEntryKind::HostProjection,
                identity,
                layout,
                capture,
                "rebuild-host-projection",
            );
        }

        push_structural(
            &mut entries,
            ImageEntryKind::RunnerDiagnostic,
            "runner::logger-state",
            "outside-schema",
            "exclude",
            "runner-owned",
        );

        push_structural(
            &mut entries,
            ImageEntryKind::Unsupported,
            "unsupported::live-scene-state",
            "scene-registry-reconstruction",
            "reject",
            "reject",
        );

        let mut identities = BTreeSet::new();
        let mut census = StateImagePlanCensus {
            examined: entries.len(),
            ..StateImagePlanCensus::default()
        };
        for entry in &entries {
            if !identities.insert(entry.identity.clone()) {
                return Err(StateImagePlanError::DuplicateIdentity(entry.identity.clone()));
            }
            census.assigned += 1;
            match entry.kind {
                ImageEntryKind::SemanticSession => census.semantic_session += 1,
                ImageEntryKind::ProgramArtifact => census.program_artifact += 1,
                ImageEntryKind::HostProjection => census.host_projection += 1,
                ImageEntryKind::RunnerDiagnostic => census.runner_diagnostic += 1,
                ImageEntryKind::Unsupported => census.unsupported += 1,
            }
        }
        Ok(Self {
            schema_id: schema.schema_id.clone(),
            entries,
            census,
        })
    }

    pub fn schema_id(&self) -> &str {
        &self.schema_id
    }

    pub fn entries(&self) -> &[StateImageEntry] {
        &self.entries
    }

    pub fn census(&self) -> &StateImagePlanCensus {
        &self.census
    }
}

fn push_structural(
    entries: &mut Vec<StateImageEntry>,
    kind: ImageEntryKind,
    identity: &str,
    layout: &str,
    capture_policy: &str,
    realization_policy: &str,
) {
    entries.push(StateImageEntry {
        identity: identity.into(),
        kind,
        layout: layout.into(),
        capture_policy: capture_policy.into(),
        realization_policy: realization_policy.into(),
    });
}

fn schema_tail(entry: &crate::state_schema::StateEntry) -> &str {
    entry.identity.rsplit("::").next().unwrap_or(&entry.identity)
}

/// State roots owned by the sim runner or its diagnostics are deliberately
/// excluded from a semantic image. This is the compiler-side spelling of the
/// existing runner boundary; logger tables are structural because they are
/// inline-host roots and appear as `runner::logger-state` above.
fn runner_global(entry: &crate::state_schema::StateEntry) -> bool {
    const ROOTS: &[&str] = &[
        "SIMS",
        "SIM_HANDLE",
        "SIM_ENTRIES",
        "SIM_ENTRY_INDEX",
        "SIM_ENTRY_FRAME_START",
        "SIM_FIXED_DT",
        "SIM_FIXED_EPOCH",
        "SIM_INPUT_SYNTHETIC_ONLY",
        "PERF_FIXED_DT_ENABLED",
        "PERF_FIXED_DT",
        "SIM_SKIP_RENDER",
        "SIM_STOP_SITE",
        "SIM_STOP_PENDING",
        "SIM_NATIVE_BOOT",
        "SIM_NATIVE_BOOT_UNRESTORABLE",
        "SIM_DIAG_BOOT_GL",
        "SIM_DIAG_BOOT_SHADER",
        "SIM_DIAG_BASE_GL",
        "SIM_DIAG_BASE_SHADER",
        "FRAME_FORCE_ERROR_AT",
        "FRAME_ERROR_COUNT",
        "JIT_TRACE_LOG_ENABLED",
        "JIT_TRACE_INSTALLED",
        "JIT_TRACE_STARTS",
        "JIT_TRACE_ABORTS",
        "JIT_TRACE_LAST_STARTS",
        "JIT_TRACE_LAST_ABORTS",
        "JIT_RUNTIME_ENABLED",
    ];
    ROOTS.contains(&schema_tail(entry))
}

fn host_global(entry: &crate::state_schema::StateEntry) -> bool {
    schema_tail(entry) == "stone_window"
}

#[cfg(test)]
mod tests {
    use super::{ImageEntryKind, StateImagePlan};
    use crate::state_schema::{StateEntry, StateKind, StateSchema};

    fn schema() -> StateSchema {
        StateSchema::from_entries(vec![
            StateEntry {
                kind: StateKind::Global,
                identity: "global:test::value".into(),
                fingerprint: "int".into(),
                physical: vec!["value".into()],
                owner: Some("test".into()),
                initializer_identity: None,
            },
            StateEntry {
                kind: StateKind::Static,
                identity: "static:test::lazy#0".into(),
                fingerprint: "int;done".into(),
                physical: vec!["cell".into(), "done".into()],
                owner: Some("test".into()),
                initializer_identity: Some(7),
            },
            StateEntry {
                kind: StateKind::CoroutineFrame,
                identity: "fn:test::frame".into(),
                fingerprint: "slots=[0]".into(),
                physical: vec!["frame:fn:test::frame".into()],
                owner: Some("fn:test::frame".into()),
                initializer_identity: None,
            },
            StateEntry {
                kind: StateKind::Storage,
                identity: "primitive:int".into(),
                fingerprint: "ctype=int;size=4".into(),
                physical: vec!["storage:int".into()],
                owner: None,
                initializer_identity: None,
            },
        ])
        .expect("fixture schema")
    }

    #[test]
    fn plan_is_total_and_classifies_every_root_once() {
        let plan = StateImagePlan::from_schema(&schema()).expect("image plan");
        let census = plan.census();
        assert!(census.examined > 0);
        assert_eq!(census.examined, census.assigned);
        assert_eq!(census.duplicates, 0);
        assert_eq!(census.unknown, 0);
        assert!(census.semantic_session > 0);
        assert!(census.program_artifact > 0);
        assert!(census.host_projection > 0);
        assert!(census.runner_diagnostic > 0);
        assert!(census.unsupported > 0);
        assert_eq!(
            plan.entries()
                .iter()
                .filter(|entry| entry.kind == ImageEntryKind::SemanticSession)
                .count(),
            census.semantic_session
        );
    }

    #[test]
    fn plan_keeps_schema_order_and_has_no_duplicate_identities() {
        let schema = schema();
        let plan = StateImagePlan::from_schema(&schema).expect("image plan");
        let schema_ids: Vec<_> = schema.entries.iter().map(|entry| entry.identity.as_str()).collect();
        let plan_schema_ids: Vec<_> = plan
            .entries()
            .iter()
            .take(schema.entries.len())
            .map(|entry| entry.identity.as_str())
            .collect();
        assert_eq!(plan_schema_ids, schema_ids);
        let mut ids = std::collections::BTreeSet::new();
        for entry in plan.entries() {
            assert!(ids.insert(entry.identity.clone()), "duplicate {}", entry.identity);
            assert!(!entry.layout.is_empty());
            assert!(!entry.capture_policy.is_empty());
            assert!(!entry.realization_policy.is_empty());
        }
    }

    #[test]
    fn empty_schema_is_rejected_before_an_empty_image_plan_can_pass() {
        let schema = StateSchema::from_entries(Vec::new()).expect("empty schema fixture");
        assert_eq!(
            StateImagePlan::from_schema(&schema),
            Err(super::StateImagePlanError::EmptySchema)
        );
    }
}
