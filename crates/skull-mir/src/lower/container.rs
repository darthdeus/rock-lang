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
}

impl ContainerPlan {
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
            ) if family == ContainerFamily::Arena && name.as_str() == "Arena" => true,
            (
                ContainerFamily::NativeBuffer,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                },
            ) if name.as_str() == "NativeBuffer" => true,
            (
                ContainerFamily::FlatVec,
                crate::monoty::MonoTy::App {
                    ctor: crate::monoty::MonoCtor::Nominal(name), ..
                },
            ) if name.as_str() == "FlatVec" => true,
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
            ) if name.as_str() == "Arena" => {
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
            ) if (family == ContainerFamily::NativeBuffer && name.as_str() == "NativeBuffer")
                || (family == ContainerFamily::FlatVec && name.as_str() == "FlatVec") => {
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
        let element_rep = arena.plan(
            ws,
            target,
            &physical_element,
            RepUse::Boundary,
            &RepUseFacts::default(),
            RepGuarantees::default(),
        )?;
        let shape = arena.value_shape(element_rep);
        let columns = match (family, shape.as_ref()) {
            (ContainerFamily::Vec, Some(shape))
                if !nullable || crate::lower::is_nobox_nominal(ws, shape.name) =>
            {
                let paths = crate::lower::value_leaf_paths(shape);
                (paths.len() + usize::from(nullable)
                    <= crate::lower::MAX_COLUMN_LEAVES)
                    .then(|| !paths.iter().any(|path| path.as_str() == "len"))
                    .unwrap_or(false)
            }
            (ContainerFamily::Map | ContainerFamily::Arena, Some(shape)) if !nullable =>
            {
                let paths = crate::lower::value_leaf_paths(shape);
                paths.len() <= crate::lower::MAX_COLUMN_LEAVES
                    && !paths.iter().any(|path| path.as_str() == "len")
            }
            _ => false,
        };
        let leaf_addressed = !nullable
            && matches!(family, ContainerFamily::NativeBuffer | ContainerFamily::FlatVec)
            && shape.is_some_and(|shape| {
                crate::lower::leaf_addr_plan(ws, shape.name).is_ok()
            });
        let storage = if columns {
            ContainerStorage::Columns { element: element_rep }
        } else if leaf_addressed {
            ContainerStorage::LeafAddressed { element: element_rep }
        } else {
            ContainerStorage::Boxed { element: element_rep }
        };
        Ok(Self { target, family, storage })
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
            | ContainerStorage::LeafAddressed { element } => element,
        }
    }

    pub fn column_elem(&self, arena: &RepArena) -> Option<crate::mir::ColumnElem> {
        let ContainerStorage::Columns { element } = self.storage else { return None };
        let shape = arena.value_shape(element)?;
        Some(crate::mir::ColumnElem { name: shape.name, nullable: shape.nullable })
    }

    pub fn ffi_element_name(
        &self,
        ws: &bone_semantic::inference::Workspace,
        arena: &RepArena,
    ) -> Option<ustr::Ustr> {
        let shape = match self.storage {
            ContainerStorage::Boxed { element }
            | ContainerStorage::Columns { element }
            | ContainerStorage::LeafAddressed { element } => arena.value_shape(element)?,
        };
        crate::lower::is_ffi_clean_value_struct(ws, shape.name).then_some(shape.name)
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
