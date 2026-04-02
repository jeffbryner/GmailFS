use fuser::FileAttr;
use moka::future::Cache;
use std::time::Duration;

pub struct MetadataCache {
    cache: Cache<u64, FileAttr>,
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

    pub async fn get(&self, ino: u64) -> Option<FileAttr> {
        self.cache.get(&ino).await
    }

    pub async fn insert(&self, ino: u64, attr: FileAttr) {
        self.cache.insert(ino, attr).await;
    }

    pub async fn invalidate(&self, ino: u64) {
        self.cache.invalidate(&ino).await;
    }
}
