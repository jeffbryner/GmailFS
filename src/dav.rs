use crate::cache::BodyCache;
use crate::gmail::GmailClient;
use bytes::Bytes;
use dashmap::DashSet;
use dav_server::davpath::DavPath;
use dav_server::fs::*;
use futures::future::FutureExt;
use futures::stream::StreamExt;
use moka::future::Cache;
use std::fmt;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, info};

const MAX_OUTBOX_FILE_SIZE: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct GmailDav {
    client: Arc<GmailClient>,
    body_cache: Arc<BodyCache>,
    path_to_id: Arc<moka::sync::Cache<String, String>>,
    active_searches: Arc<DashSet<String>>,
    tombstones: Arc<DashSet<String>>,
    api_semaphore: Arc<Semaphore>,
    dir_cache: Arc<Cache<String, Vec<(String, bool, SystemTime)>>>,
}

impl fmt::Debug for GmailDav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailDav")
            .field("active_searches", &self.active_searches)
            .finish()
    }
}

impl GmailDav {
    pub fn new(client: Arc<GmailClient>, body_cache: Arc<BodyCache>) -> Self {
        Self {
            client,
            body_cache,
            path_to_id: Arc::new(
                moka::sync::Cache::builder()
                    .time_to_live(Duration::from_secs(86400))
                    .max_capacity(10000)
                    .build(),
            ),
            active_searches: Arc::new(DashSet::new()),
            tombstones: Arc::new(DashSet::new()),
            api_semaphore: Arc::new(Semaphore::new(10)),
            dir_cache: Arc::new(
                Cache::builder()
                    .time_to_live(Duration::from_secs(30))
                    .build(),
            ),
        }
    }

    fn resolve_id(&self, display_name: &str) -> Option<String> {
        if let Some(id) = self.path_to_id.get(display_name) {
            return Some(id);
        }
        let parts: Vec<&str> = display_name.split('_').collect();
        if parts.len() >= 3 {
            let last = parts.last().unwrap();
            if last.len() >= 15 {
                return Some(last.to_string());
            }
        }
        None
    }

    async fn get_content_bytes(&self, path: &DavPath) -> FsResult<Bytes> {
        let rel_path = path.as_rel_ospath();
        let rel_path_str = rel_path.to_str().unwrap_or("");
        if self.tombstones.contains(rel_path_str) {
            return Err(FsError::NotFound);
        }

        let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();

        let (msg_display_name, file_name, is_attachment) =
            if parts.len() == 3 && (parts[0] == "inbox" || parts[0] == "unread") {
                (parts[1], parts[2], false)
            } else if parts.len() == 4 && parts[0] == "search" {
                (parts[2], parts[3], false)
            } else if parts.len() == 4
                && (parts[0] == "inbox" || parts[0] == "unread")
                && parts[2] == "attachments"
            {
                (parts[1], parts[3], true)
            } else if parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments" {
                (parts[2], parts[4], true)
            } else {
                return Err(FsError::NotFound);
            };

        let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;

        let cache_key = if is_attachment {
            format!("{}:att:{}", msg_id, file_name)
        } else {
            format!("{}:{}", msg_id, file_name)
        };

        let client = self.client.clone();
        let msg_id_clone = msg_id.clone();
        let file_name_clone = file_name.to_string();
        let semaphore = self.api_semaphore.clone();

        self.body_cache
            .get_or_insert_with(cache_key.clone(), move || async move {
                let _permit = semaphore.acquire().await.map_err(|_| {
                    error!("Failed to acquire API semaphore");
                    FsError::GeneralFailure
                })?;
                if is_attachment {
                    info!(
                        "Starting live download for attachment: {} from message {}",
                        file_name_clone, msg_id_clone
                    );
                    let atts = client
                        .get_attachments_list(&msg_id_clone)
                        .await
                        .map_err(|e| {
                            error!("Failed to list attachments: {}", e);
                            FsError::GeneralFailure
                        })?;
                    let att = atts
                        .into_iter()
                        .find(|a| a.name == file_name_clone)
                        .ok_or(FsError::NotFound)?;
                    let data = client
                        .get_attachment_data(&msg_id_clone, &att.attachment_id)
                        .await
                        .map_err(|e| {
                            error!("Attachment download failed: {}", e);
                            FsError::GeneralFailure
                        })?;
                    info!("Download complete: {} bytes", data.len());
                    Ok(data)
                } else {
                    match file_name_clone.as_str() {
                        "body.md" => client
                            .get_message_markdown_bytes(&msg_id_clone)
                            .await
                            .map_err(|e| {
                                error!("MD fetch failed: {}", e);
                                FsError::GeneralFailure
                            }),
                        "body.html" => client
                            .get_message_html_bytes(&msg_id_clone)
                            .await
                            .map_err(|_| FsError::GeneralFailure),
                        "snippet.txt" => client
                            .get_message_snippet_bytes(&msg_id_clone)
                            .await
                            .map_err(|_| FsError::GeneralFailure),
                        "metadata.json" => client
                            .get_message_metadata_bytes(&msg_id_clone)
                            .await
                            .map_err(|_| FsError::GeneralFailure),
                        _ => Err(FsError::NotFound),
                    }
                }
            })
            .await
    }
}

impl DavFileSystem for GmailDav {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();

            if parts.len() == 2 && parts[0] == "outbox" && (options.write || options.create) {
                return Ok(Box::new(GmailDavFile {
                    dav: self.clone(),
                    path: path.clone(),
                    content: None,
                    size: None,
                    modified: SystemTime::now(),
                    pos: 0,
                    write_buffer: Some(Arc::new(Mutex::new(Vec::new()))),
                }) as Box<dyn DavFile>);
            }

            // Lazy loading: verify existence and get metadata first,
            // but return a file handle without pre-fetching content bytes.
            // Full download happens on-demand in read_bytes/seek/metadata.
            let meta = self.metadata(path).await?;

            // If the metadata returned a dummy size (1024), we set size to None
            // so that DavFile::metadata will trigger a real download to get the true size
            // for the Content-Length header during GET requests.
            let size = if meta.len() == 1024 {
                None
            } else {
                Some(meta.len())
            };

            Ok(Box::new(GmailDavFile {
                dav: self.clone(),
                path: path.clone(),
                content: None,
                size,
                modified: meta.modified().unwrap_or_else(|_| SystemTime::now()),
                pos: 0,
                write_buffer: None,
            }) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            info!("read_dir: path={:?} parts={:?}", rel_path, parts);

            if let Some(cached_entries) = self.dir_cache.get(rel_path_str).await {
                let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
                for (name, is_dir, modified) in cached_entries {
                    entries.push(Box::new(GmailDavDirEntry::new_with_time(
                        &name, is_dir, modified,
                    )));
                }

                let filtered_entries: Vec<_> = entries
                    .into_iter()
                    .filter(|e| {
                        let name = String::from_utf8_lossy(&e.name()).to_string();
                        let full_child_path = if rel_path_str.is_empty() {
                            name
                        } else {
                            format!("{}/{}", rel_path_str, name)
                        };
                        !self.tombstones.contains(&full_child_path)
                    })
                    .collect();

                let stream = futures::stream::iter(filtered_entries.into_iter().map(Ok));
                return Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>);
            }

            let mut raw_entries: Vec<(String, bool, SystemTime)> = Vec::new();

            if parts.is_empty() {
                let now = SystemTime::now();
                raw_entries.push(("00_MOUNT_CHECK_OK".to_string(), false, now));
                raw_entries.push(("inbox".to_string(), true, now));
                raw_entries.push(("unread".to_string(), true, now));
                raw_entries.push(("outbox".to_string(), true, now));
                raw_entries.push(("search".to_string(), true, now));
                raw_entries.push(("saved_searches".to_string(), true, now));
            } else if (parts[0] == "inbox" || parts[0] == "unread") && parts.len() == 1 {
                let message_stubs = {
                    let _permit = self.api_semaphore.acquire().await;
                    if parts[0] == "inbox" {
                        self.client.list_inbox_messages(100).await
                    } else {
                        self.client.list_unread_messages(100).await
                    }
                }
                .map_err(|_| FsError::GeneralFailure)?;

                let mut detail_futures = futures::stream::iter(message_stubs)
                    .map(|stub| {
                        let client = self.client.clone();
                        let semaphore = self.api_semaphore.clone();
                        async move {
                            let _permit = semaphore.acquire().await;
                            client
                                .get_message(stub.id.as_deref().unwrap_or_default())
                                .await
                        }
                    })
                    .buffer_unordered(10);

                while let Some(msg_res) = detail_futures.next().await {
                    if let Ok(msg) = msg_res {
                        let display_name = self.client.get_display_name(&msg);
                        self.path_to_id
                            .insert(display_name.clone(), msg.id.clone().unwrap_or_default());

                        let modified = if let Some(internal_date) = msg.internal_date {
                            SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                        } else {
                            SystemTime::now()
                        };

                        raw_entries.push((display_name, true, modified));
                    }
                }
            } else if parts[0] == "outbox" && parts.len() == 1 {
                // Outbox is normally empty until someone writes to it
            } else if (parts[0] == "search" || parts[0] == "saved_searches") && parts.len() == 1 {
                let now = SystemTime::now();
                if parts[0] == "search" {
                    raw_entries.push(("example-query".to_string(), true, now));
                }
                for query in self.active_searches.iter() {
                    raw_entries.push((query.key().clone(), true, now));
                }
            } else if parts[0] == "search" && parts.len() == 2 {
                let query = parts[1];
                info!("Executing live search for: {}", query);
                let message_stubs = {
                    let _permit = self.api_semaphore.acquire().await;
                    self.client.search_messages(query).await
                }
                .map_err(|_| FsError::GeneralFailure)?;

                let mut detail_futures = futures::stream::iter(message_stubs)
                    .map(|stub| {
                        let client = self.client.clone();
                        let semaphore = self.api_semaphore.clone();
                        async move {
                            let _permit = semaphore.acquire().await;
                            client
                                .get_message(stub.id.as_deref().unwrap_or_default())
                                .await
                        }
                    })
                    .buffer_unordered(10);

                while let Some(msg_res) = detail_futures.next().await {
                    if let Ok(msg) = msg_res {
                        let display_name = self.client.get_display_name(&msg);
                        self.path_to_id
                            .insert(display_name.clone(), msg.id.clone().unwrap_or_default());

                        let modified = if let Some(internal_date) = msg.internal_date {
                            SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                        } else {
                            SystemTime::now()
                        };

                        raw_entries.push((display_name, true, modified));
                    }
                }
            } else if (parts.len() == 2 && (parts[0] == "inbox" || parts[0] == "unread"))
                || (parts.len() == 3 && parts[0] == "search")
            {
                let msg_display_name = if parts[0] == "search" {
                    parts[2]
                } else {
                    parts[1]
                };

                let modified = if let Some(id) = self.resolve_id(msg_display_name) {
                    if let Ok(msg) = self.client.get_message(&id).await {
                        if let Some(internal_date) = msg.internal_date {
                            SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                        } else {
                            SystemTime::now()
                        }
                    } else {
                        SystemTime::now()
                    }
                } else {
                    SystemTime::now()
                };

                raw_entries.push(("body.md".to_string(), false, modified));
                raw_entries.push(("body.html".to_string(), false, modified));
                raw_entries.push(("snippet.txt".to_string(), false, modified));
                raw_entries.push(("metadata.json".to_string(), false, modified));
                raw_entries.push(("attachments".to_string(), true, modified));
            } else if (parts.len() == 3
                && (parts[0] == "inbox" || parts[0] == "unread")
                && parts[2] == "attachments")
                || (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments")
            {
                let msg_display_name = if parts[0] == "search" {
                    parts[2]
                } else {
                    parts[1]
                };
                let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;

                let modified = if let Ok(msg) = self.client.get_message(&msg_id).await {
                    if let Some(internal_date) = msg.internal_date {
                        SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                    } else {
                        SystemTime::now()
                    }
                } else {
                    SystemTime::now()
                };

                let atts = {
                    let _permit = self.api_semaphore.acquire().await;
                    self.client
                        .get_attachments_list(&msg_id)
                        .await
                        .map_err(|_| FsError::GeneralFailure)?
                };
                for att in atts {
                    raw_entries.push((att.name, false, modified));
                }
            }

            self.dir_cache
                .insert(rel_path_str.to_string(), raw_entries.clone())
                .await;

            let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
            for (name, is_dir, modified) in raw_entries {
                entries.push(Box::new(GmailDavDirEntry::new_with_time(
                    &name, is_dir, modified,
                )));
            }

            let filtered_entries: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    let name = String::from_utf8_lossy(&e.name()).to_string();
                    let full_child_path = if rel_path_str.is_empty() {
                        name
                    } else {
                        format!("{}/{}", rel_path_str, name)
                    };
                    !self.tombstones.contains(&full_child_path)
                })
                .collect();

            let stream = futures::stream::iter(filtered_entries.into_iter().map(Ok));
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");

            if self.tombstones.contains(rel_path_str) {
                return Err(FsError::NotFound);
            }

            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();

            if !parts.is_empty() {
                let last = parts.last().unwrap();
                if last.starts_with("._") || *last == ".DS_Store" {
                    return Err(FsError::NotFound);
                }
            }

            let mut is_dir = false;
            let mut is_file = false;

            if parts.is_empty() {
                is_dir = true;
            } else if parts.len() == 1
                && (parts[0] == "inbox"
                    || parts[0] == "unread"
                    || parts[0] == "search"
                    || parts[0] == "saved_searches"
                    || parts[0] == "outbox")
            {
                is_dir = true;
            } else if parts.len() == 2 && (parts[0] == "inbox" || parts[0] == "unread") {
                is_dir = true;
            } else if parts.len() == 2 && (parts[0] == "search" || parts[0] == "saved_searches") {
                is_dir = parts[1] == "example-query" || self.active_searches.contains(parts[1]);
            } else if parts.len() == 3 && parts[0] == "search" {
                is_dir = true;
            } else if (parts.len() == 3
                && (parts[0] == "inbox" || parts[0] == "unread")
                && parts[2] == "attachments")
                || (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments")
            {
                is_dir = true;
            } else if parts.len() == 1 && parts[0] == "00_MOUNT_CHECK_OK" {
                is_file = true;
            } else if parts.len() == 2 && parts[0] == "outbox" {
                is_file = true;
            } else if (parts.len() == 3 && (parts[0] == "inbox" || parts[0] == "unread"))
                || (parts.len() == 4 && parts[0] == "search")
            {
                is_file = true;
            } else if (parts.len() == 4
                && (parts[0] == "inbox" || parts[0] == "unread")
                && parts[2] == "attachments")
                || (parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments")
            {
                is_file = true;
            }

            if is_dir {
                let modified = if parts.len() == 2 && (parts[0] == "inbox" || parts[0] == "unread") {
                    if let Some(id) = self.resolve_id(parts[1]) {
                        if let Ok(msg) = self.client.get_message(&id).await {
                            if let Some(internal_date) = msg.internal_date {
                                SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                            } else {
                                SystemTime::now()
                            }
                        } else {
                            SystemTime::now()
                        }
                    } else {
                        SystemTime::now()
                    }
                } else if parts.len() == 3 && parts[0] == "search" {
                    if let Some(id) = self.resolve_id(parts[2]) {
                        if let Ok(msg) = self.client.get_message(&id).await {
                            if let Some(internal_date) = msg.internal_date {
                                SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                            } else {
                                SystemTime::now()
                            }
                        } else {
                            SystemTime::now()
                        }
                    } else {
                        SystemTime::now()
                    }
                } else if (parts.len() == 3
                    && (parts[0] == "inbox" || parts[0] == "unread")
                    && parts[2] == "attachments")
                    || (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments")
                {
                    let msg_display_name = if parts[0] == "search" {
                        parts[2]
                    } else {
                        parts[1]
                    };
                    if let Some(id) = self.resolve_id(msg_display_name) {
                        if let Ok(msg) = self.client.get_message(&id).await {
                            if let Some(internal_date) = msg.internal_date {
                                SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                            } else {
                                SystemTime::now()
                            }
                        } else {
                            SystemTime::now()
                        }
                    } else {
                        SystemTime::now()
                    }
                } else {
                    SystemTime::now()
                };
                Ok(Box::new(GmailDavMetaData::new_with_time(true, 0, modified)) as Box<dyn DavMetaData>)
            } else if is_file {
                if parts[0] == "00_MOUNT_CHECK_OK" {
                    return Ok(Box::new(GmailDavMetaData::new(false, 2)) as Box<dyn DavMetaData>);
                }

                if parts[0] == "outbox" {
                    return Ok(Box::new(GmailDavMetaData::new(false, 0)) as Box<dyn DavMetaData>);
                }

                let (msg_display_name, file_name, is_attachment) = if (parts[0] == "inbox"
                    || parts[0] == "unread")
                    && parts.len() == 4
                    && parts[2] == "attachments"
                {
                    (parts[1], parts[3], true)
                } else if parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments" {
                    (parts[2], parts[4], true)
                } else if (parts[0] == "inbox" || parts[0] == "unread") && parts.len() == 3 {
                    (parts[1], parts[2], false)
                } else if parts.len() == 4 && parts[0] == "search" {
                    (parts[2], parts[3], false)
                } else {
                    ("", "", false)
                };

                let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
                let modified = if let Ok(msg) = self.client.get_message(&msg_id).await {
                    if let Some(internal_date) = msg.internal_date {
                        SystemTime::UNIX_EPOCH + Duration::from_millis(internal_date as u64)
                    } else {
                        SystemTime::now()
                    }
                } else {
                    SystemTime::now()
                };

                if is_attachment {
                    let atts = {
                        let _permit = self.api_semaphore.acquire().await;
                        self.client
                            .get_attachments_list(&msg_id)
                            .await
                            .map_err(|_| FsError::GeneralFailure)?
                    };
                    let att = atts
                        .into_iter()
                        .find(|a| a.name == file_name)
                        .ok_or(FsError::NotFound)?;
                    return Ok(Box::new(GmailDavMetaData::new_with_time(
                        false, att.size, modified,
                    )) as Box<dyn DavMetaData>);
                }

                if file_name == "body.md"
                    || file_name == "body.html"
                    || file_name == "snippet.txt"
                    || file_name == "metadata.json"
                {
                    return Ok(Box::new(GmailDavMetaData::new_with_time(
                        false, 1024, modified,
                    )) as Box<dyn DavMetaData>);
                }

                Err(FsError::NotFound)
            } else {
                Err(FsError::NotFound)
            }
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();

            info!("create_dir: path={:?} parts={:?}", rel_path, parts);
            self.tombstones.remove(rel_path_str);

            if parts.len() == 2 && (parts[0] == "search" || parts[0] == "saved_searches") {
                let query = parts[1].to_string();
                if !self.active_searches.contains(&query) {
                    info!("Registered magic search node: {}", query);
                    self.active_searches.insert(query);
                    self.dir_cache.invalidate("search").await;
                    self.dir_cache.invalidate("saved_searches").await;
                }
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            info!("remove_dir: path={:?} parts={:?}", rel_path, parts);

            if parts.len() == 2 && (parts[0] == "search" || parts[0] == "saved_searches") {
                self.active_searches.remove(parts[1]);
                // No tombstone needed; removal from active_searches + cache invalidation is enough
                self.dir_cache.invalidate("search").await;
                self.dir_cache.invalidate("saved_searches").await;
                Ok(())
            } else if (parts.len() == 2 && (parts[0] == "inbox" || parts[0] == "unread"))
                || (parts.len() == 3 && parts[0] == "search")
            {
                let msg_display_name = if parts[0] == "search" {
                    parts[2]
                } else {
                    parts[1]
                };
                let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
                self.client.trash_message(&msg_id).await.map_err(|e| {
                    error!("Trash failed: {}", e);
                    FsError::GeneralFailure
                })?;

                // No tombstone needed for the folder itself as it's gone from the API
                let to_remove: Vec<String> = self
                    .tombstones
                    .iter()
                    .filter(|p| p.starts_with(&format!("{}/", rel_path_str)))
                    .map(|p| p.clone())
                    .collect();
                for p in to_remove {
                    self.tombstones.remove(&p);
                }

                // Invalidate parent directory cache
                if parts[0] == "search" {
                    self.dir_cache
                        .invalidate(&format!("search/{}", parts[1]))
                        .await;
                } else {
                    self.dir_cache.invalidate(parts[0]).await;
                }
                Ok(())
            } else if ((parts[0] == "inbox" || parts[0] == "unread")
                && parts.len() == 3
                && parts[2] == "attachments")
                || (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments")
            {
                // Deleting the "attachments" folder itself
                self.tombstones.insert(rel_path_str.to_string());
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            info!("remove_file: path={:?} parts={:?}", rel_path, parts);

            if ((parts[0] == "inbox" || parts[0] == "unread") && parts.len() == 3)
                || (parts.len() == 4 && parts[0] == "search")
                || ((parts[0] == "inbox" || parts[0] == "unread")
                    && parts.len() == 4
                    && parts[2] == "attachments")
                || (parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments")
                || (parts[0] == "outbox")
            {
                self.tombstones.insert(rel_path_str.to_string());
                self.dir_cache.invalidate(parts[0]).await;
                if parts[0] == "search" && parts.len() >= 2 {
                    self.dir_cache
                        .invalidate(&format!("{}/{}", parts[0], parts[1]))
                        .await;
                }
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, _to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = from.as_rel_ospath();
            let parts: Vec<&str> = rel_path
                .to_str()
                .unwrap_or("")
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();

            if parts.len() == 2 && (parts[0] == "inbox" || parts[0] == "unread") {
                let msg_id = self.resolve_id(parts[1]).ok_or(FsError::NotFound)?;
                self.client
                    .archive_message(&msg_id)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }
}

#[derive(Debug, Clone)]
struct GmailDavMetaData {
    is_dir: bool,
    size: u64,
    modified: SystemTime,
}

impl GmailDavMetaData {
    fn new(is_dir: bool, size: u64) -> Self {
        Self {
            is_dir,
            size,
            modified: SystemTime::now(),
        }
    }

    fn new_with_time(is_dir: bool, size: u64, modified: SystemTime) -> Self {
        Self {
            is_dir,
            size,
            modified,
        }
    }
}

impl DavMetaData for GmailDavMetaData {
    fn len(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

struct GmailDavDirEntry {
    name: String,
    is_dir: bool,
    modified: SystemTime,
}

impl GmailDavDirEntry {
    fn new_with_time(name: &str, is_dir: bool, modified: SystemTime) -> Self {
        Self {
            name: name.to_string(),
            is_dir,
            modified,
        }
    }
}

impl DavDirEntry for GmailDavDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            Ok(Box::new(GmailDavMetaData::new_with_time(
                self.is_dir,
                0,
                self.modified,
            )) as Box<dyn DavMetaData>)
        }
        .boxed()
    }
}

#[derive(Debug)]
struct GmailDavFile {
    dav: GmailDav,
    path: DavPath,
    content: Option<Bytes>,
    size: Option<u64>,
    modified: SystemTime,
    pos: usize,
    write_buffer: Option<Arc<Mutex<Vec<u8>>>>,
}

impl DavFile for GmailDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            if let Some(content) = &self.content {
                Ok(Box::new(GmailDavMetaData::new_with_time(
                    false,
                    content.len() as u64,
                    self.modified,
                )) as Box<dyn DavMetaData>)
            } else if let Some(size) = self.size {
                Ok(Box::new(GmailDavMetaData::new_with_time(
                    false,
                    size,
                    self.modified,
                )) as Box<dyn DavMetaData>)
            } else if self.write_buffer.is_some() {
                Ok(Box::new(GmailDavMetaData::new_with_time(
                    false,
                    0,
                    self.modified,
                )) as Box<dyn DavMetaData>)
            } else {
                // If we reach here, we are likely handling a GET request for a file
                // where we only had a dummy size. We MUST download the real content
                // now to provide an accurate Content-Length header, otherwise
                // the file will be truncated by the client.
                let content = self.dav.get_content_bytes(&self.path).await?;
                let size = content.len() as u64;
                self.content = Some(content);
                Ok(Box::new(GmailDavMetaData::new_with_time(
                    false,
                    size,
                    self.modified,
                )) as Box<dyn DavMetaData>)
            }
        }
        .boxed()
    }

    fn write_buf(&mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        async move {
            if let Some(buffer) = &self.write_buffer {
                let mut buffer = buffer.lock().await;
                while buf.has_remaining() {
                    let chunk = buf.chunk();
                    if buffer.len() + chunk.len() > MAX_OUTBOX_FILE_SIZE {
                        return Err(FsError::TooLarge);
                    }
                    buffer.extend_from_slice(chunk);
                    let len = chunk.len();
                    buf.advance(len);
                }
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        async move {
            if let Some(buffer) = &self.write_buffer {
                let mut buffer = buffer.lock().await;
                if buffer.len() + buf.len() > MAX_OUTBOX_FILE_SIZE {
                    return Err(FsError::TooLarge);
                }
                buffer.extend_from_slice(&buf);
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self.content.is_none() {
                self.content = Some(self.dav.get_content_bytes(&self.path).await?);
            }
            let content = self.content.as_ref().unwrap();
            let start = self.pos;
            let end = std::cmp::min(start + count, content.len());
            let chunk = content.slice(start..end);
            self.pos = end;
            debug!(
                "read_bytes: pos={} count={} returning={}",
                start,
                count,
                chunk.len()
            );
            Ok(chunk)
        }
        .boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            if self.content.is_none() && self.write_buffer.is_none() {
                self.content = Some(self.dav.get_content_bytes(&self.path).await?);
            }
            if let Some(content) = &self.content {
                let new_pos = match pos {
                    SeekFrom::Start(p) => p as i64,
                    SeekFrom::Current(p) => self.pos as i64 + p,
                    SeekFrom::End(p) => content.len() as i64 + p,
                };
                if new_pos < 0 {
                    return Err(FsError::Forbidden);
                }
                self.pos = std::cmp::min(new_pos as usize, content.len());
                debug!("seek: new_pos={}", self.pos);
                Ok(self.pos as u64)
            } else {
                Ok(0)
            }
        }
        .boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move {
            if let Some(buffer) = &self.write_buffer {
                let data = buffer.lock().await;
                let content = String::from_utf8_lossy(&data).to_string();

                // Parse headers and body
                let mut to = String::new();
                let mut subject = String::new();
                let mut body = String::new();
                let mut parsing_headers = true;

                for line in content.lines() {
                    if parsing_headers {
                        if line.is_empty() {
                            parsing_headers = false;
                            continue;
                        }
                        if line.to_lowercase().starts_with("to:") {
                            to = line[3..].trim().to_string();
                        } else if line.to_lowercase().starts_with("subject:") {
                            subject = line[8..].trim().to_string();
                        }
                    } else {
                        body.push_str(line);
                        body.push('\n');
                    }
                }

                if !to.is_empty() {
                    info!("Flush triggered send to: {}", to);
                    self.dav
                        .client
                        .send_email(&to, &subject, &body)
                        .await
                        .map_err(|e| {
                            error!("Send failed: {}", e);
                            FsError::GeneralFailure
                        })?;
                }
            }
            Ok(())
        }
        .boxed()
    }
}
