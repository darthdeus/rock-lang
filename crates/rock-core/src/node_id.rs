use crate::prelude::*;

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct AstNodeId(pub ModuleId, pub u32);

impl AstNodeId {
    pub fn new(module_id: ModuleId, local_id: u32) -> Self {
        Self(module_id, local_id)
    }

    /// An id in the builtin module, for call sites that carry only a number.
    pub fn from_u32(local: u32) -> Self {
        Self(ModuleId::BUILTIN, local)
    }

    pub fn module_id(&self) -> ModuleId {
        self.0
    }

    pub fn local_id(&self) -> u32 {
        self.1
    }
}

#[derive(Debug)]
pub struct AstNodeIdGen {
    module_id: ModuleId,
    next_id: AtomicU32,
}

impl AstNodeIdGen {
    pub fn new(module_id: ModuleId) -> Self {
        Self {
            module_id,
            next_id: AtomicU32::new(0),
        }
    }

    /// A generator whose first id is `base`, so ids stay deterministic across
    /// compilation units that were split apart.
    pub fn new_with_base(module_id: ModuleId, base_id: u32) -> Self {
        Self {
            module_id,
            next_id: AtomicU32::new(base_id),
        }
    }

    pub fn id_gen(&self) -> AstNodeId {
        let local_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        AstNodeId(self.module_id, local_id)
    }
}

#[derive(Debug)]
pub struct AstBuilder {
    id_gen: AstNodeIdGen,
    hole_counter: AtomicU32,
}

impl AstBuilder {
    pub fn new_for_module(module_id: ModuleId) -> Self {
        Self {
            id_gen: AstNodeIdGen::new(module_id),
            hole_counter: AtomicU32::new(0),
        }
    }

    pub fn new_with_base(module_id: ModuleId, base_id: u32) -> Self {
        Self {
            id_gen: AstNodeIdGen::new_with_base(module_id, base_id),
            hole_counter: AtomicU32::new(0),
        }
    }

    pub fn gen_id(&self) -> AstNodeId {
        self.id_gen.id_gen()
    }

    pub fn current_id(&self) -> u32 {
        self.id_gen
            .next_id
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn gen_hole_id(&self) -> u32 {
        self.hole_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct FunctionId(pub AstNodeId);

#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct GlobalId(AstNodeId);

impl GlobalId {
    pub fn ast_id(&self) -> AstNodeId {
        self.0
    }

    pub fn from_global_ast_id_unchecked(ast_id: AstNodeId) -> Self {
        Self(ast_id)
    }
}
