use moka::future::Cache;
use std::time::Duration;

pub struct MetadataCache {
    // We'll store a simple size and is_dir bool for WebDAV
    cache: Cache<String, (u64, bool)>,
}

impl MetadataCache {
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .max_capacity(10_000)
                .build(),
        }
    }

    pub async fn get(&self, path: String) -> Option<(u64, bool)> {
        self.cache.get(&path).await
    }

    pub async fn insert(&self, path: String, val: (u64, bool)) {
        self.cache.insert(path, val).await;
    }

    pub async fn invalidate(&self, path: String) {
        self.cache.invalidate(&path).await;
    }
}

pub struct BodyCache {
    cache: Cache<String, String>,
}

impl BodyCache {
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .max_capacity(100)
                .build(),
        }
    }

    pub async fn get(&self, id: String) -> Option<String> {
        self.cache.get(&id).await
    }

    pub async fn insert(&self, id: String, body: String) {
        self.cache.insert(id, body).await;
    }
}
