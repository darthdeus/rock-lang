use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContainerFamily {
    Vec,
    Map,
    Arena,
    NativeBuffer,
    FlatVec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerStorage {
    Boxed { element: RepId },
    Columns { element: RepId },
    Strided { element: RepId },
    LeafAddressed { element: RepId },
}

/// An address-bearing place whose element representation was selected by the
/// owning [`ContainerPlan`]. `LeafAddr` supplies operational offsets; the
/// RepId supplies the static identity those offsets conform to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceBundle {
    representation: RepId,
    address: LeafAddr,
}

impl PlaceBundle {
    pub fn representation(&self) -> RepId {
        self.representation
    }

    pub fn address(&self) -> &LeafAddr {
        &self.address
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerPlan {
    target: bone_semantic::hir::FnBackend,
    family: ContainerFamily,
    storage: ContainerStorage,
    leaf_addresses: Option<Vec<LeafAddr>>,
}

impl ContainerPlan {
    pub(crate) fn into_row_storage(mut self) -> Self {
        match self.storage {
            ContainerStorage::Columns { element }
            | ContainerStorage::Strided { element }
            | ContainerStorage::LeafAddressed { element } => {
                self.storage = ContainerStorage::Boxed { element };
                self.leaf_addresses = None;
            }
            ContainerStorage::Boxed { .. } => {}
        }
        self
    }

    pub fn matches_type(family: ContainerFamily, ty: &crate::monoty::MonoTy) -> bool {
        match (family, ty.peel_nullable().0) {
            (
                ContainerFamily::Vec,
                crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Array, .. },
            ) => true,
            (
                ContainerFamily::Map,
                crate::monoty::MonoTy::App { ctor: crate::monoty::MonoCtor::Map, .. },
            ) => true,
            (
                ContainerFamily::Arena,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                },
            ) if family == ContainerFamily::Arena
                && bone_semantic::prelude::is_intrinsic_key(*name, "Arena") => true,
            (
                ContainerFamily::NativeBuffer,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                },
            ) if bone_semantic::prelude::is_intrinsic_key(*name, "NativeBuffer") => true,
            (
                ContainerFamily::FlatVec,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                },
            ) if bone_semantic::prelude::is_intrinsic_key(*name, "FlatVec") => true,
            _ => false,
        }
    }

    pub fn for_type(
        ws: &bone_semantic::inference::Workspace,
        target: bone_semantic::hir::FnBackend,
        arena: &RepArena,
        family: ContainerFamily,
        ty: &crate::monoty::MonoTy,
    ) -> Result<Self, RepError> {
        let (element, nullable) = match (family, ty.peel_nullable().0) {
            (
                ContainerFamily::Vec,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Array,
                    params,
                },
            ) => {
                let element = params.first().ok_or_else(|| RepError {
                    span: None,
                    message: "Vec has no element type".into(),
                })?;
                let (payload, nullable) = element.peel_nullable();
                (payload.clone(), nullable)
            }
            (
                ContainerFamily::Map,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Map,
                    params,
                },
            ) => {
                let value = params.get(1).ok_or_else(|| RepError {
                    span: None,
                    message: "Map has no value type".into(),
                })?;
                let (value, _) = value.peel_nullable();
                (value.clone(), false)
            }
            (
                ContainerFamily::Arena,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    params,
                },
            ) if bone_semantic::prelude::is_intrinsic_key(*name, "Arena") => {
                let element = params.first().ok_or_else(|| RepError {
                    span: None,
                    message: "Arena has no element type".into(),
                })?;
                let (element, nullable) = element.peel_nullable();
                (element.clone(), nullable)
            }
            (
                family @ (ContainerFamily::NativeBuffer | ContainerFamily::FlatVec),
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    params,
                },
            ) if (family == ContainerFamily::NativeBuffer
                && bone_semantic::prelude::is_intrinsic_key(*name, "NativeBuffer"))
                || (family == ContainerFamily::FlatVec
                    && bone_semantic::prelude::is_intrinsic_key(*name, "FlatVec")) => {
                let element = params.first().ok_or_else(|| RepError {
                    span: None,
                    message: "native buffer has no element type".into(),
                })?;
                let (element, nullable) = element.peel_nullable();
                (element.clone(), nullable)
            }
            _ => {
                return Err(RepError {
                    span: None,
                    message: format!("{} is not a {:?} container", ty.render(), family),
                })
            }
        };
        let physical_element = if nullable {
            crate::monoty::MonoTy::Nullable(Box::new(element.clone()))
        } else {
            element.clone()
        };
        let element_rep = arena.plan_container_element(ws, target, &physical_element)?;
        let shape = arena.value_shape(element_rep);
        let column_eligible = arena.column_eligible(element_rep);
        let dense_strided = matches!(family, ContainerFamily::Vec)
            && !nullable
            && match &element {
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } => crate::lower::dense_flat_enum_vec_eligible(ws, *name),
                _ => false,
            };
        // A wide `nobox` value struct stores row-major in one table: a column
        // per leaf costs a table lookup, bounds check and barrier per leaf on
        // every push and read, which is what made a 50-leaf push end hot traces
        // at LuaJIT's constant limit (docs/nobox-wide-trace-plan.md). Only a
        // non-generic struct: reflection finds a row's layout by struct name
        // (`__ROCK_STRIDED_ROWS`), which names one layout only without params.
        let wide_nobox_struct = matches!(family, ContainerFamily::Vec)
            && !nullable
            && column_eligible
            && arena.get(element_rep).nobox()
            && matches!(
                &element,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    params,
                } if params.is_empty() && ws.struct_by_name.contains_key(name)
            )
            && shape
                .as_ref()
                .is_some_and(|shape| shape.leaf_count() > crate::lower::MAX_COLUMN_LEAVES);
        let columns = match (family, shape.as_ref()) {
            (ContainerFamily::Vec, _) if wide_nobox_struct => false,
            (ContainerFamily::Vec, Some(shape)) if column_eligible => {
                let paths = crate::lower::value_leaf_paths(shape);
                (paths.len() + usize::from(nullable) <= crate::lower::MAX_COLUMN_LEAVES
                    || arena.get(element_rep).nobox())
                    .then(|| !paths.iter().any(|path| path.as_str() == "len"))
                    .unwrap_or(false)
            }
            (ContainerFamily::Map | ContainerFamily::Arena, Some(shape))
                if !nullable && column_eligible =>
            {
                let paths = crate::lower::value_leaf_paths(shape);
                paths.len() <= crate::lower::MAX_COLUMN_LEAVES
                    && !paths.iter().any(|path| path.as_str() == "len")
            }
            _ => false,
        };
        let leaf_addresses = if !nullable
            && matches!(family, ContainerFamily::NativeBuffer | ContainerFamily::FlatVec)
        {
            arena.leaf_addresses(ws, element_rep)?
        } else {
            None
        };
        let leaf_addressed = leaf_addresses.is_some();
        let storage = if columns {
            ContainerStorage::Columns { element: element_rep }
        } else if dense_strided || wide_nobox_struct {
            ContainerStorage::Strided { element: element_rep }
        } else if leaf_addressed {
            ContainerStorage::LeafAddressed { element: element_rep }
        } else {
            ContainerStorage::Boxed { element: element_rep }
        };
        Ok(Self { target, family, storage, leaf_addresses })
    }

    /// Leaf path -> 1-based row offset for a `[name]` whose element is a
    /// non-generic struct stored as strided rows, or `None` for any other
    /// storage. Reflection reads rows through this; it is the plan lowering
    /// stores by, not a restatement of it.
    pub fn strided_struct_row_offsets(
        ws: &bone_semantic::inference::Workspace,
        target: bone_semantic::hir::FnBackend,
        name: ustr::Ustr,
    ) -> Option<Vec<(ustr::Ustr, usize)>> {
        use crate::monoty::{MonoCtor, MonoTy};
        if !ws.struct_by_name.contains_key(&name) {
            return None;
        }
        let ty = MonoTy::App {
            ctor: MonoCtor::Array,
            params: vec![MonoTy::App { ctor: MonoCtor::Nominal(name), params: Vec::new() }],
        };
        let arena = RepArena::default();
        let plan = Self::for_type(ws, target, &arena, ContainerFamily::Vec, &ty).ok()?;
        let shape = arena.value_shape(plan.strided_elem(&arena)?.representation())?;
        Some(
            crate::lower::value_leaf_paths(&shape)
                .into_iter()
                .enumerate()
                .map(|(i, path)| (path, i + 1))
                .collect(),
        )
    }

    pub fn target(&self) -> bone_semantic::hir::FnBackend {
        self.target
    }

    pub fn family(&self) -> ContainerFamily {
        self.family
    }

    pub fn storage(&self) -> &ContainerStorage {
        &self.storage
    }

    pub fn element_rep(&self) -> RepId {
        match self.storage {
            ContainerStorage::Boxed { element }
            | ContainerStorage::Columns { element }
            | ContainerStorage::Strided { element }
            | ContainerStorage::LeafAddressed { element } => element,
        }
    }

    pub fn element_shape(&self, arena: &RepArena) -> Option<ValueShape> {
        arena.value_shape(self.element_rep())
    }

    pub fn column_elem(&self, arena: &RepArena) -> Option<crate::mir::ColumnElem> {
        let ContainerStorage::Columns { element } = self.storage else { return None };
        let shape = arena.value_shape(element)?;
        Some(crate::mir::ColumnElem::planned(
            shape.name,
            shape.nullable,
            self.element_rep(),
            arena.get(self.element_rep()).nobox(),
        ))
    }

    pub fn strided_elem(&self, arena: &RepArena) -> Option<crate::mir::ColumnElem> {
        let ContainerStorage::Strided { element } = self.storage else { return None };
        let shape = arena.value_shape(element)?;
        Some(crate::mir::ColumnElem::planned(
            shape.name,
            shape.nullable,
            self.element_rep(),
            arena.get(self.element_rep()).nobox(),
        ))
    }

    pub fn ffi_element_name(&self, arena: &RepArena) -> Option<ustr::Ustr> {
        let shape = self.element_shape(arena)?;
        arena.column_eligible(self.element_rep()).then_some(shape.name)
    }

    pub fn leaf_addresses(&self) -> Option<&[LeafAddr]> {
        self.leaf_addresses.as_deref()
    }

    pub fn is_leaf_addressed(&self) -> bool {
        matches!(self.storage, ContainerStorage::LeafAddressed { .. })
    }

    pub fn place(&self, address: LeafAddr) -> Result<PlaceBundle, RepError> {
        if !self.is_leaf_addressed() {
            return Err(RepError {
                span: None,
                message: "a non-leaf-addressed container cannot produce a leaf place".into(),
            });
        }
        Ok(PlaceBundle { representation: self.element_rep(), address })
    }
}
