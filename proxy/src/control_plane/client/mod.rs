pub mod cplane_proxy_v1;
#[cfg(any(test, feature = "testing"))]
pub mod mock;

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use clashmap::ClashMap;
use tokio::time::Instant;
use tracing::{debug, info};

use super::{EndpointAccessControl, RoleAccessControl};
use crate::auth::backend::ComputeUserInfo;
use crate::auth::backend::jwt::{AuthRule, FetchAuthRules, FetchAuthRulesError};
use crate::cache::node_info::{CachedNodeInfo, NodeInfoCache};
use crate::cache::project_info::ProjectInfoCache;
use crate::config::{CacheOptions, ProjectInfoCacheOptions};
use crate::context::RequestContext;
use crate::control_plane::{ControlPlaneApi, errors};
use crate::error::ReportableError;
use crate::metrics::ApiLockMetrics;
use crate::rate_limiter::{DynamicLimiter, Outcome, RateLimiterConfig, Token};
use crate::types::EndpointId;

#[non_exhaustive]
#[derive(Clone)]
pub enum ControlPlaneClient {
    /// Proxy V1 control plane API
    ProxyV1(cplane_proxy_v1::NeonControlPlaneClient),
    /// Local mock control plane.
    #[cfg(any(test, feature = "testing"))]
    PostgresMock(mock::MockControlPlane),
    /// Internal testing
    #[cfg(test)]
    #[allow(private_interfaces)]
    Test(Box<dyn TestControlPlaneClient>),
}

impl ControlPlaneApi for ControlPlaneClient {
    async fn get_role_access_control(
        &self,
        ctx: &RequestContext,
        endpoint: &EndpointId,
        role: &crate::types::RoleName,
    ) -> Result<RoleAccessControl, errors::GetAuthInfoError> {
        match self {
            Self::ProxyV1(api) => api.get_role_access_control(ctx, endpoint, role).await,
            #[cfg(any(test, feature = "testing"))]
            Self::PostgresMock(api) => api.get_role_access_control(ctx, endpoint, role).await,
            #[cfg(test)]
            Self::Test(_api) => {
                unreachable!("this function should never be called in the test backend")
            }
        }
    }

    async fn get_endpoint_access_control(
        &self,
        ctx: &RequestContext,
        endpoint: &EndpointId,
        role: &crate::types::RoleName,
    ) -> Result<EndpointAccessControl, errors::GetAuthInfoError> {
        match self {
            Self::ProxyV1(api) => api.get_endpoint_access_control(ctx, endpoint, role).await,
            #[cfg(any(test, feature = "testing"))]
            Self::PostgresMock(api) => api.get_endpoint_access_control(ctx, endpoint, role).await,
            #[cfg(test)]
            Self::Test(api) => api.get_access_control(),
        }
    }

    async fn get_endpoint_jwks(
        &self,
        ctx: &RequestContext,
        endpoint: &EndpointId,
    ) -> Result<Vec<AuthRule>, errors::GetEndpointJwksError> {
        match self {
            Self::ProxyV1(api) => api.get_endpoint_jwks(ctx, endpoint).await,
            #[cfg(any(test, feature = "testing"))]
            Self::PostgresMock(api) => api.get_endpoint_jwks(ctx, endpoint).await,
            #[cfg(test)]
            Self::Test(_api) => Ok(vec![]),
        }
    }

    async fn wake_compute(
        &self,
        ctx: &RequestContext,
        user_info: &ComputeUserInfo,
    ) -> Result<CachedNodeInfo, errors::WakeComputeError> {
        match self {
            Self::ProxyV1(api) => api.wake_compute(ctx, user_info).await,
            #[cfg(any(test, feature = "testing"))]
            Self::PostgresMock(api) => api.wake_compute(ctx, user_info).await,
            #[cfg(test)]
            Self::Test(api) => api.wake_compute(),
        }
    }
}

#[cfg(test)]
pub(crate) trait TestControlPlaneClient: Send + Sync + 'static {
    fn wake_compute(&self) -> Result<CachedNodeInfo, errors::WakeComputeError>;

    fn get_access_control(&self) -> Result<EndpointAccessControl, errors::GetAuthInfoError>;

    fn dyn_clone(&self) -> Box<dyn TestControlPlaneClient>;
}

#[cfg(test)]
impl Clone for Box<dyn TestControlPlaneClient> {
    fn clone(&self) -> Self {
        TestControlPlaneClient::dyn_clone(&**self)
    }
}

/// Various caches for [`control_plane`](super).
pub struct ApiCaches {
    /// Cache for the `wake_compute` API method.
    pub(crate) node_info: NodeInfoCache,
    /// Cache which stores project_id -> endpoint_ids mapping.
    pub project_info: Arc<ProjectInfoCache>,
}

impl ApiCaches {
    pub fn new(
        wake_compute_cache_config: CacheOptions,
        project_info_cache_config: ProjectInfoCacheOptions,
    ) -> Self {
        Self {
            node_info: NodeInfoCache::new(wake_compute_cache_config),
            project_info: Arc::new(ProjectInfoCache::new(project_info_cache_config)),
        }
    }
}

/// Various caches for [`control_plane`](super).
pub struct ApiLocks<K> {
    name: &'static str,
    node_locks: ClashMap<K, Arc<DynamicLimiter>>,
    config: RateLimiterConfig,
    timeout: Duration,
    epoch: std::time::Duration,
    metrics: &'static ApiLockMetrics,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiLockError {
    #[error("timeout acquiring resource permit")]
    TimeoutError(#[from] tokio::time::error::Elapsed),
}

impl ReportableError for ApiLockError {
    fn get_error_kind(&self) -> crate::error::ErrorKind {
        match self {
            ApiLockError::TimeoutError(_) => crate::error::ErrorKind::RateLimit,
        }
    }
}

impl<K: Hash + Eq + Clone> ApiLocks<K> {
    pub fn new(
        name: &'static str,
        config: RateLimiterConfig,
        shards: usize,
        timeout: Duration,
        epoch: std::time::Duration,
        metrics: &'static ApiLockMetrics,
    ) -> Self {
        Self {
            name,
            node_locks: ClashMap::with_shard_amount(shards),
            config,
            timeout,
            epoch,
            metrics,
        }
    }

    pub(crate) async fn get_permit(&self, key: &K) -> Result<WakeComputePermit, ApiLockError> {
        if self.config.initial_limit == 0 {
            return Ok(WakeComputePermit {
                permit: Token::disabled(),
            });
        }
        let now = Instant::now();
        let semaphore = {
            // get fast path
            if let Some(semaphore) = self.node_locks.get(key) {
                semaphore.clone()
            } else {
                self.node_locks
                    .entry(key.clone())
                    .or_insert_with(|| {
                        self.metrics.semaphores_registered.inc();
                        DynamicLimiter::new(self.config)
                    })
                    .clone()
            }
        };
        let permit = semaphore.acquire_timeout(self.timeout).await;

        self.metrics
            .semaphore_acquire_seconds
            .observe(now.elapsed().as_secs_f64());

        if permit.is_ok() {
            debug!(elapsed = ?now.elapsed(), "acquired permit");
        } else {
            debug!(elapsed = ?now.elapsed(), "timed out acquiring permit");
        }
        Ok(WakeComputePermit { permit: permit? })
    }

    pub async fn garbage_collect_worker(&self) {
        if self.config.initial_limit == 0 {
            return;
        }
        let mut interval =
            tokio::time::interval(self.epoch / (self.node_locks.shards().len()) as u32);
        loop {
            for (i, shard) in self.node_locks.shards().iter().enumerate() {
                interval.tick().await;
                // temporary lock a single shard and then clear any semaphores that aren't currently checked out
                // race conditions: if strong_count == 1, there's no way that it can increase while the shard is locked
                // therefore releasing it is safe from race conditions
                info!(
                    name = self.name,
                    shard = i,
                    "performing epoch reclamation on api lock"
                );
                let mut lock = shard.write();
                let timer = self.metrics.reclamation_lag_seconds.start_timer();
                let count = lock
                    .extract_if(|(_, semaphore)| Arc::strong_count(semaphore) == 1)
                    .count();
                drop(lock);
                self.metrics.semaphores_unregistered.inc_by(count as u64);
                timer.observe();
            }
        }
    }
}

pub(crate) struct WakeComputePermit {
    permit: Token,
}

impl WakeComputePermit {
    pub(crate) fn should_check_cache(&self) -> bool {
        !self.permit.is_disabled()
    }
    pub(crate) fn release(self, outcome: Outcome) {
        self.permit.release(outcome);
    }
    pub(crate) fn release_result<T, E>(self, res: Result<T, E>) -> Result<T, E> {
        match res {
            Ok(_) => self.release(Outcome::Success),
            Err(_) => self.release(Outcome::Overload),
        }
        res
    }
}

impl FetchAuthRules for ControlPlaneClient {
    async fn fetch_auth_rules(
        &self,
        ctx: &RequestContext,
        endpoint: EndpointId,
    ) -> Result<Vec<AuthRule>, FetchAuthRulesError> {
        self.get_endpoint_jwks(ctx, &endpoint)
            .await
            .map_err(FetchAuthRulesError::GetEndpointJwks)
    }
}
