use crate::{ModelId, ProviderId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteDecision {
    ThirdParty {
        provider_id: ProviderId,
        upstream_model_id: ModelId,
    },
    BuiltInOfficial,
    UnavailableManagedModel,
}

pub trait RouteResolver {
    fn resolve(&self, catalog_model: &str) -> RouteDecision;
}
