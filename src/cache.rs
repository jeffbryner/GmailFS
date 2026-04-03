use moka::future::Cache;
use std::time::Duration;
use bytes::Bytes;
use std::future::Future;
use dav_server::fs::{FsResult, FsError};
use std::fmt;

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
                .time_to_live(Duration::from_secs(600))
                .max_capacity(200)
                .build(),
        }
    }

    pub async fn get_or_insert_with<F, Fut>(&self, key: String, f: F) -> FsResult<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = FsResult<Bytes>>,
    {
        let res = self.cache.get_with(key, async move {
            f().await.unwrap_or_else(|_| Bytes::new())
        }).await;

        if res.is_empty() {
            Err(FsError::GeneralFailure)
        } else {
            Ok(res)
        }
    }
}
