use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContainerFamily {
    Vec,
    Map,
    Arena,
    NativeBuffer,
    FlatVec,
}

impl ContainerFamily {
    /// The family of `ty` itself (a nullable container is not one) and its type arguments.
    pub fn of(ty: &crate::monoty::MonoTy) -> Option<(Self, &[crate::monoty::MonoTy])> {
        use crate::monoty::{MonoCtor, MonoTy};
        let MonoTy::App { ctor, params } = ty else {
            return None;
        };
        let family = match ctor {
            MonoCtor::Array => Self::Vec,
            MonoCtor::Map => Self::Map,
            MonoCtor::Nominal(name) => {
                [
                    ("Arena", Self::Arena),
                    ("NativeBuffer", Self::NativeBuffer),
                    ("FlatVec", Self::FlatVec),
                ]
                .into_iter()
                .find(|(key, _)| bone_core::modules::is_intrinsic_key(*name, key))?
                .1
            }
            _ => return None,
        };
        Some((family, params))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerStorage {
    Boxed { element: RepId },
    Columns { element: RepId },
    Strided { element: RepId },
    LeafAddressed { element: RepId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerPlan {
    target: bone_core::hir::FnBackend,
    family: ContainerFamily,
    storage: ContainerStorage,
    leaf_addresses: Option<Vec<LeafAddr>>,
}

impl ContainerPlan {
    pub fn matches_type(family: ContainerFamily, ty: &crate::monoty::MonoTy) -> bool {
        ContainerFamily::of(ty.peel_nullable().0).is_some_and(|(found, _)| found == family)
    }

    pub fn for_type(
        ws: &bone_core::workspace::Workspace,
        target: bone_core::hir::FnBackend,
        arena: &RepArena,
        family: ContainerFamily,
        ty: &crate::monoty::MonoTy,
    ) -> Result<Self, RepError> {
        arena.container_plan(target, family, ty, || {
            Self::for_type_uncached(ws, target, arena, family, ty)
        })
    }

    fn for_type_uncached(
        ws: &bone_core::workspace::Workspace,
        target: bone_core::hir::FnBackend,
        arena: &RepArena,
        family: ContainerFamily,
        ty: &crate::monoty::MonoTy,
    ) -> Result<Self, RepError> {
        let params = match ContainerFamily::of(ty.peel_nullable().0) {
            Some((found, params)) if found == family => params,
            _ => {
                return Err(RepError {
                    span: None,
                    message: format!("{} is not a {:?} container", ty.render(), family),
                });
            }
        };
        // A Map stores its values under keys, so a nullable value needs no presence.
        let element_index = usize::from(family == ContainerFamily::Map);
        let element = params.get(element_index).ok_or_else(|| RepError {
            span: None,
            message: format!("{family:?} has no element type"),
        })?;
        let (element, nullable) = element.peel_nullable();
        let (element, nullable) = (element.clone(), nullable && family != ContainerFamily::Map);
        let physical_element = if nullable {
            crate::monoty::MonoTy::nullable(element.clone())
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
                } => {
                    crate::lower::dense_flat_enum_vec_eligible(ws, *name)
                        || (target == bone_core::hir::FnBackend::Lua
                            && crate::lower::is_json_value(ws, *name)
                            && shape.as_ref().is_some_and(|shape| shape.leaf_count() == 2))
                }
                _ => false,
            };
        // A column per leaf costs a lookup, bounds check and barrier per leaf, which
        // ended hot traces at LuaJIT's constant limit for wide structs.
        // A generic field stays one opaque leaf, so type arguments do not move row offsets.
        let wide_nobox_struct = matches!(family, ContainerFamily::Vec | ContainerFamily::Map)
            && !nullable
            && column_eligible
            && arena.get(element_rep).nobox()
            && matches!(
                &element,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name),
                    ..
                } if ws.nominals.struct_id(*name).is_some()
            )
            && shape
                .as_ref()
                .is_some_and(|shape| shape.leaf_count() > crate::lower::MAX_COLUMN_LEAVES);
        let columns = match (family, shape.as_ref()) {
            (_, _) if wide_nobox_struct => false,
            (ContainerFamily::Vec | ContainerFamily::Map | ContainerFamily::Arena, Some(shape))
                if column_eligible && (!nullable || family == ContainerFamily::Vec) =>
            {
                let paths = crate::lower::value_leaf_paths(shape);
                (family != ContainerFamily::Arena && arena.get(element_rep).nobox())
                    || (paths.len() + usize::from(nullable) <= crate::lower::MAX_COLUMN_LEAVES
                        && !paths.iter().any(|path| path.as_str() == "len"))
            }
            _ => false,
        };
        let leaf_addresses = if !nullable
            && matches!(
                family,
                ContainerFamily::NativeBuffer | ContainerFamily::FlatVec
            ) {
            arena.leaf_addresses(ws, element_rep)?
        } else {
            None
        };
        let leaf_addressed = leaf_addresses.is_some();
        let storage = if columns {
            ContainerStorage::Columns {
                element: element_rep,
            }
        } else if dense_strided || wide_nobox_struct {
            ContainerStorage::Strided {
                element: element_rep,
            }
        } else if leaf_addressed {
            ContainerStorage::LeafAddressed {
                element: element_rep,
            }
        } else {
            ContainerStorage::Boxed {
                element: element_rep,
            }
        };
        Ok(Self {
            target,
            family,
            storage,
            leaf_addresses,
        })
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
        arena.value_shape(self.element_rep()).cloned()
    }

    pub fn column_elem(&self, arena: &RepArena) -> Option<crate::mir::ColumnElem> {
        match self.flat_elem(arena)? {
            (elem, FlatStorage::Columns) => Some(elem),
            (_, FlatStorage::Strided) => None,
        }
    }

    pub(crate) fn flat_elem(
        &self,
        arena: &RepArena,
    ) -> Option<(crate::mir::ColumnElem, FlatStorage)> {
        let (element, storage) = match self.storage {
            ContainerStorage::Columns { element } => (element, FlatStorage::Columns),
            ContainerStorage::Strided { element } => (element, FlatStorage::Strided),
            ContainerStorage::Boxed { .. } | ContainerStorage::LeafAddressed { .. } => {
                return None;
            }
        };
        let shape = arena.value_shape(element)?;
        let elem = crate::mir::ColumnElem::planned(
            shape.name,
            shape.nullable,
            element,
            arena.get(element).nobox(),
        );
        Some((elem, storage))
    }

    pub(crate) fn ffi_element_name(&self, arena: &RepArena) -> Option<ustr::Ustr> {
        let shape = self.element_shape(arena)?;
        arena
            .column_eligible(self.element_rep())
            .then_some(shape.name)
    }

    pub fn leaf_places(&self) -> Option<&[LeafAddr]> {
        self.leaf_addresses
            .as_deref()
            .filter(|_| self.is_leaf_addressed())
    }

    pub fn is_leaf_addressed(&self) -> bool {
        matches!(self.storage, ContainerStorage::LeafAddressed { .. })
    }
}
