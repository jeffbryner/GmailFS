use std::sync::Arc;
use std::time::SystemTime;
use std::io::SeekFrom;
use dav_server::fs::*;
use dav_server::davpath::DavPath;
use futures::future::FutureExt;
use crate::gmail::GmailClient;
use crate::cache::BodyCache;
use tracing::{error, info, debug};
use bytes::Bytes;
use dashmap::{DashMap, DashSet};
use futures::stream::StreamExt;

#[derive(Clone)]
pub struct GmailDav {
    client: Arc<GmailClient>,
    body_cache: Arc<BodyCache>,
    path_to_id: Arc<DashMap<String, String>>,
    active_searches: Arc<DashSet<String>>,
}

impl GmailDav {
    pub fn new(client: Arc<GmailClient>, body_cache: Arc<BodyCache>) -> Self {
        Self { 
            client, 
            body_cache,
            path_to_id: Arc::new(DashMap::new()),
            active_searches: Arc::new(DashSet::new()),
        }
    }

    fn resolve_id(&self, display_name: &str) -> Option<String> {
        if let Some(id) = self.path_to_id.get(display_name) {
            return Some(id.clone());
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
        let parts: Vec<&str> = rel_path.to_str().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
        
        let (msg_display_name, file_name, is_attachment) = if parts.len() == 3 && parts[0] == "inbox" {
            (parts[1], parts[2], false)
        } else if parts.len() == 4 && parts[0] == "search" {
            (parts[2], parts[3], false)
        } else if parts.len() == 4 && parts[0] == "inbox" && parts[2] == "attachments" {
            (parts[1], parts[3], true)
        } else if parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments" {
            (parts[2], parts[4], true)
        } else {
            return Err(FsError::NotFound);
        };

        let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
        
        if is_attachment {
            let atts = self.client.get_attachments_list(&msg_id).await
                .map_err(|_| FsError::GeneralFailure)?;
            let att = atts.into_iter().find(|a| a.name == file_name).ok_or(FsError::NotFound)?;
            return self.client.get_attachment_data(&msg_id, &att.attachment_id).await
                .map_err(|_| FsError::GeneralFailure);
        }

        let cache_key = format!("{}:{}", msg_id, file_name);
        if let Some(cached) = self.body_cache.get(cache_key.clone()).await {
            return Ok(Bytes::from(cached));
        }

        let res = match file_name {
            "body.md" => self.client.get_message_markdown(&msg_id).await
                .map_err(|e| { error!("MD fetch failed: {}", e); FsError::GeneralFailure }),
            "body.html" => self.client.get_message_html(&msg_id).await
                .map_err(|_| FsError::GeneralFailure),
            "snippet.txt" => self.client.get_message_snippet(&msg_id).await
                .map_err(|_| FsError::GeneralFailure),
            "metadata.json" => self.client.get_message_metadata(&msg_id).await
                .map_err(|_| FsError::GeneralFailure),
            _ => Err(FsError::NotFound),
        }?;

        self.body_cache.insert(cache_key, res.clone()).await;
        Ok(Bytes::from(res))
    }
}

impl DavFileSystem for GmailDav {
    fn open<'a>(&'a self, path: &'a DavPath, _options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let content = self.get_content_bytes(path).await?;
            Ok(Box::new(GmailDavFile { content }) as Box<dyn DavFile>)
        }.boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let rel_path = path.as_rel_ospath();
            let parts: Vec<&str> = rel_path.to_str().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
            info!("read_dir: path={:?} parts={:?}", rel_path, parts);

            let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();

            if parts.is_empty() {
                entries.push(Box::new(GmailDavDirEntry::new("00_MOUNT_CHECK_OK", false)));
                entries.push(Box::new(GmailDavDirEntry::new("inbox", true)));
                entries.push(Box::new(GmailDavDirEntry::new("search", true)));
            } else if parts[0] == "inbox" && parts.len() == 1 {
                let message_stubs = self.client.list_inbox_messages(20).await
                    .map_err(|_| FsError::GeneralFailure)?;
                
                let mut detail_futures = futures::stream::iter(message_stubs)
                    .map(|stub| {
                        let client = self.client.clone();
                        async move { client.get_message(stub.id.as_deref().unwrap_or_default()).await }
                    })
                    .buffer_unordered(10);

                while let Some(msg_res) = detail_futures.next().await {
                    if let Ok(msg) = msg_res {
                        let display_name = self.client.get_display_name(&msg);
                        self.path_to_id.insert(display_name.clone(), msg.id.clone().unwrap_or_default());
                        entries.push(Box::new(GmailDavDirEntry::new(&display_name, true)));
                    }
                }
            } else if parts[0] == "search" && parts.len() == 1 {
                entries.push(Box::new(GmailDavDirEntry::new("example-query", true)));
                for query in self.active_searches.iter() {
                    entries.push(Box::new(GmailDavDirEntry::new(query.key(), true)));
                }
            } else if parts[0] == "search" && parts.len() == 2 {
                let query = parts[1];
                info!("Executing live search for: {}", query);
                let message_stubs = self.client.search_messages(query).await
                    .map_err(|_| FsError::GeneralFailure)?;
                
                let mut detail_futures = futures::stream::iter(message_stubs)
                    .map(|stub| {
                        let client = self.client.clone();
                        async move { client.get_message(stub.id.as_deref().unwrap_or_default()).await }
                    })
                    .buffer_unordered(10);

                while let Some(msg_res) = detail_futures.next().await {
                    if let Ok(msg) = msg_res {
                        let display_name = self.client.get_display_name(&msg);
                        self.path_to_id.insert(display_name.clone(), msg.id.clone().unwrap_or_default());
                        entries.push(Box::new(GmailDavDirEntry::new(&display_name, true)));
                    }
                }
            } else if (parts[0] == "inbox" && parts.len() == 2) || (parts[0] == "search" && parts.len() == 3) {
                entries.push(Box::new(GmailDavDirEntry::new("body.md", false)));
                entries.push(Box::new(GmailDavDirEntry::new("body.html", false)));
                entries.push(Box::new(GmailDavDirEntry::new("snippet.txt", false)));
                entries.push(Box::new(GmailDavDirEntry::new("metadata.json", false)));
                entries.push(Box::new(GmailDavDirEntry::new("attachments", true)));
            } else if (parts[0] == "inbox" && parts.len() == 3 && parts[2] == "attachments") || 
                      (parts[0] == "search" && parts.len() == 4 && parts[3] == "attachments") {
                let msg_display_name = if parts[0] == "inbox" { parts[1] } else { parts[2] };
                let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
                let atts = self.client.get_attachments_list(&msg_id).await
                    .map_err(|_| FsError::GeneralFailure)?;
                for att in atts {
                    entries.push(Box::new(GmailDavDirEntry::new(&att.name, false)));
                }
            }

            let stream = futures::stream::iter(entries.into_iter().map(Ok));
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        }.boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let rel_path = path.as_rel_ospath();
            let parts: Vec<&str> = rel_path.to_str().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();

            // Ignore macOS metadata files explicitly
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
            } else if parts.len() == 1 && (parts[0] == "inbox" || parts[0] == "search") {
                is_dir = true;
            } else if parts[0] == "inbox" && parts.len() == 2 {
                is_dir = true;
            } else if parts[0] == "search" && parts.len() == 2 {
                is_dir = parts[1] == "example-query" || self.active_searches.contains(parts[1]);
            } else if parts[0] == "search" && parts.len() == 3 {
                is_dir = true;
            } else if (parts.len() == 3 && parts[2] == "attachments") || (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments") {
                is_dir = true;
            } else if parts.len() == 1 && parts[0] == "00_MOUNT_CHECK_OK" {
                is_file = true;
            } else if (parts.len() == 3 && parts[0] == "inbox") || (parts.len() == 4 && parts[0] == "search") {
                is_file = true;
            } else if (parts.len() == 4 && parts[0] == "inbox" && parts[2] == "attachments") || (parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments") {
                is_file = true;
            }

            info!("metadata: path={:?} parts={:?} is_dir={} is_file={}", rel_path, parts, is_dir, is_file);

            if is_dir {
                Ok(Box::new(GmailDavMetaData::new(true, 0)) as Box<dyn DavMetaData>)
            } else if is_file {
                if parts[0] == "00_MOUNT_CHECK_OK" {
                    return Ok(Box::new(GmailDavMetaData::new(false, 2)) as Box<dyn DavMetaData>);
                }

                let (msg_display_name, file_name, is_attachment) = if parts.len() == 4 && parts[0] == "inbox" {
                    (parts[1], parts[3], true)
                } else if parts.len() == 5 && parts[0] == "search" {
                    (parts[2], parts[4], true)
                } else {
                    ("", "", false)
                };

                if is_attachment {
                    let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
                    let atts = self.client.get_attachments_list(&msg_id).await.map_err(|_| FsError::GeneralFailure)?;
                    let att = atts.into_iter().find(|a| a.name == file_name).ok_or(FsError::NotFound)?;
                    return Ok(Box::new(GmailDavMetaData::new(false, att.size)) as Box<dyn DavMetaData>);
                }

                let content = self.get_content_bytes(path).await?;
                Ok(Box::new(GmailDavMetaData::new(false, content.len() as u64)) as Box<dyn DavMetaData>)
            } else {
                Err(FsError::NotFound)
            }
        }.boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let parts: Vec<&str> = rel_path.to_str().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
            
            info!("create_dir: path={:?} parts={:?}", rel_path, parts);

            if parts.len() == 2 && parts[0] == "search" {
                let query = parts[1].to_string();
                if !self.active_searches.contains(&query) {
                    info!("Registered magic search node: {}", query);
                    self.active_searches.insert(query);
                }
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }.boxed()
    }
}

#[derive(Debug, Clone)]
struct GmailDavMetaData {
    is_dir: bool,
    size: u64,
}

impl GmailDavMetaData {
    fn new(is_dir: bool, size: u64) -> Self {
        Self { is_dir, size }
    }
}

impl DavMetaData for GmailDavMetaData {
    fn len(&self) -> u64 { self.size }
    fn modified(&self) -> FsResult<SystemTime> { Ok(SystemTime::now()) }
    fn is_dir(&self) -> bool { self.is_dir }
}

struct GmailDavDirEntry {
    name: String,
    is_dir: bool,
}

impl GmailDavDirEntry {
    fn new(name: &str, is_dir: bool) -> Self {
        Self { name: name.to_string(), is_dir }
    }
}

impl DavDirEntry for GmailDavDirEntry {
    fn name(&self) -> Vec<u8> { self.name.as_bytes().to_vec() }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            Ok(Box::new(GmailDavMetaData::new(self.is_dir, 0)) as Box<dyn DavMetaData>)
        }.boxed()
    }
}

#[derive(Debug)]
struct GmailDavFile {
    content: Bytes,
}

impl DavFile for GmailDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            Ok(Box::new(GmailDavMetaData::new(false, self.content.len() as u64)) as Box<dyn DavMetaData>)
        }.boxed()
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let end = std::cmp::min(count, self.content.len());
            Ok(self.content.slice(..end))
        }.boxed()
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        async move { Ok(0) }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}
