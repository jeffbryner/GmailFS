use moka::future::Cache;
use std::time::Duration;

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
