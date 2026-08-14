use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CatalogModelId, CoreError, ModelCacheDocument, ModelPublicationStatus, ProvidersDocument,
    RouteDecision, RouteResolver,
};

#[derive(Clone, Debug)]
struct ThirdPartyRoute {
    provider_id: crate::ProviderId,
    upstream_model_id: crate::ModelId,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeSnapshot {
    routes: BTreeMap<String, ThirdPartyRoute>,
}

impl RuntimeSnapshot {
    /// Builds an immutable routing snapshot from validated Provider configuration and cache data.
    ///
    /// # Errors
    ///
    /// Returns an error for stale/missing caches, invalid catalog identities, duplicate models,
    /// or an invalid input document.
    pub fn build(
        providers: &ProvidersDocument,
        cache: &ModelCacheDocument,
    ) -> Result<Self, CoreError> {
        providers.validate()?;
        cache.validate()?;

        let mut routes = BTreeMap::new();
        for provider in providers
            .providers
            .iter()
            .filter(|provider| provider.enabled)
        {
            let provider_cache =
                cache
                    .providers
                    .get(&provider.id)
                    .ok_or_else(|| CoreError::MissingModelCache {
                        provider_id: provider.id.to_string(),
                    })?;

            if provider_cache.config_fingerprint != provider.routing_fingerprint()? {
                return Err(CoreError::StaleModelCache {
                    provider_id: provider.id.to_string(),
                });
            }

            let mut upstream_ids = BTreeSet::new();
            for model in &provider_cache.models {
                if !upstream_ids.insert(model.upstream_model_id.clone()) {
                    return Err(CoreError::DuplicateModel {
                        provider_id: provider.id.to_string(),
                        model_id: model.upstream_model_id.to_string(),
                    });
                }

                let expected = CatalogModelId::for_provider(&provider.id, &model.upstream_model_id);
                if model.catalog_model_id != expected {
                    return Err(CoreError::CatalogModelIdMismatch {
                        expected: expected.to_string(),
                        actual: model.catalog_model_id.to_string(),
                    });
                }

                if model.publication_status != ModelPublicationStatus::Ready {
                    continue;
                }
                routes.insert(
                    expected.to_string(),
                    ThirdPartyRoute {
                        provider_id: provider.id.clone(),
                        upstream_model_id: model.upstream_model_id.clone(),
                    },
                );
            }
        }
        Ok(Self { routes })
    }

    #[must_use]
    pub fn published_model_count(&self) -> usize {
        self.routes.len()
    }
}

impl RouteResolver for RuntimeSnapshot {
    fn resolve(&self, catalog_model: &str) -> RouteDecision {
        if let Some(route) = self.routes.get(catalog_model) {
            return RouteDecision::ThirdParty {
                provider_id: route.provider_id.clone(),
                upstream_model_id: route.upstream_model_id.clone(),
            };
        }

        if catalog_model.contains('/') {
            RouteDecision::UnavailableManagedModel
        } else {
            RouteDecision::BuiltInOfficial
        }
    }
}
