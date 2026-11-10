//! Registry of core modules keyed by their declared [`ModuleId`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use shtmcl_domain::{CommandRequest, CommandResponse, ErrorEnvelope, ModuleId};

use crate::CoreModule;

#[derive(Clone, Default)]
pub struct ModuleRegistry {
    modules: BTreeMap<ModuleId, Arc<dyn CoreModule>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    Duplicate(ModuleId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(module) => write!(f, "module {module} is already registered"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one module. Registration is explicit and idempotent only in the
    /// sense that a duplicate is rejected rather than silently replacing the
    /// existing owner.
    pub fn register(&mut self, module: Arc<dyn CoreModule>) -> Result<(), RegistryError> {
        let id = module.id();
        if self.modules.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.modules.insert(id, module);
        Ok(())
    }

    pub fn get(&self, id: ModuleId) -> Option<&Arc<dyn CoreModule>> {
        self.modules.get(&id)
    }

    pub fn dispatch(
        &self,
        id: ModuleId,
        request: &CommandRequest,
    ) -> Result<CommandResponse, ErrorEnvelope> {
        let module = self.get(id).ok_or_else(ErrorEnvelope::module_unavailable)?;
        module.handle(request)
    }

    pub fn ids(&self) -> impl Iterator<Item = ModuleId> + '_ {
        self.modules.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}
