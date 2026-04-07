use bytes::Bytes;
use dav_server::fs::{FsError, FsResult};
use moka::future::Cache;
use std::fmt;
use std::future::Future;
use std::time::Duration;

pub struct BodyCache {
    cache: Cache<String, Bytes>,
}

impl fmt::Debug for BodyCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyCache").finish()
    }
}

impl BodyCache {
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                // Cache for 24 hours since email content is static
                .time_to_live(Duration::from_secs(86400))
                // Max 256 MB of cached bodies/attachments
                .weigher(|_k, v: &Bytes| v.len().try_into().unwrap_or(u32::MAX))
                .max_capacity(256 * 1024 * 1024)
                .build(),
        }
    }

    pub async fn get_or_insert_with<F, Fut>(&self, key: String, f: F) -> FsResult<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = FsResult<Bytes>>,
    {
        let res = self
            .cache
            .get_with(
                key,
                async move { f().await.unwrap_or_else(|_| Bytes::new()) },
            )
            .await;

        if res.is_empty() {
            Err(FsError::GeneralFailure)
        } else {
            Ok(res)
        }
    }
}
