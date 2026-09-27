mod compiler;
mod discovery;
mod error;
mod manual_refresh;
mod model_registry;

pub use compiler::{CatalogOverlay, CompiledCatalog, compile_catalog};
pub use discovery::ManualDiscoveryClient;
pub use error::CatalogError;
pub use manual_refresh::{
    ModelCapabilitySettings, RefreshPreview, build_refresh_preview, set_model_enabled,
    update_model_capabilities,
};
pub use model_registry::{
    MODEL_REGISTRY_SCHEMA_VERSION, MODEL_REGISTRY_URL, ModelRegistryCache, RegistryEnrichment,
};
