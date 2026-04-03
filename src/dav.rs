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
use std::fmt;

#[derive(Clone)]
pub struct GmailDav {
    client: Arc<GmailClient>,
    body_cache: Arc<BodyCache>,
    path_to_id: Arc<DashMap<String, String>>,
    active_searches: Arc<DashSet<String>>,
    // Paths that have been "virtually deleted" by the OS
    tombstones: Arc<DashSet<String>>,
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
            path_to_id: Arc::new(DashMap::new()),
            active_searches: Arc::new(DashSet::new()),
            tombstones: Arc::new(DashSet::new()),
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
        if self.tombstones.contains(rel_path.to_str().unwrap_or("")) {
            return Err(FsError::NotFound);
        }

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
        
        let cache_key = if is_attachment {
            format!("{}:att:{}", msg_id, file_name)
        } else {
            format!("{}:{}", msg_id, file_name)
        };

        let client = self.client.clone();
        let msg_id_clone = msg_id.clone();
        let file_name_clone = file_name.to_string();

        self.body_cache.get_or_insert_with(cache_key.clone(), move || async move {
            if is_attachment {
                info!("Starting live download for attachment: {} from message {}", file_name_clone, msg_id_clone);
                let atts = client.get_attachments_list(&msg_id_clone).await
                    .map_err(|e| { error!("Failed to list attachments: {}", e); FsError::GeneralFailure })?;
                let att = atts.into_iter().find(|a| a.name == file_name_clone).ok_or(FsError::NotFound)?;
                let data = client.get_attachment_data(&msg_id_clone, &att.attachment_id).await
                    .map_err(|e| { error!("Attachment download failed: {}", e); FsError::GeneralFailure })?;
                info!("Download complete: {} bytes", data.len());
                Ok(data)
            } else {
                match file_name_clone.as_str() {
                    "body.md" => client.get_message_markdown_bytes(&msg_id_clone).await
                        .map_err(|e| { error!("MD fetch failed: {}", e); FsError::GeneralFailure }),
                    "body.html" => client.get_message_html_bytes(&msg_id_clone).await
                        .map_err(|_| FsError::GeneralFailure),
                    "snippet.txt" => client.get_message_snippet_bytes(&msg_id_clone).await
                        .map_err(|_| FsError::GeneralFailure),
                    "metadata.json" => client.get_message_metadata_bytes(&msg_id_clone).await
                        .map_err(|_| FsError::GeneralFailure),
                    _ => Err(FsError::NotFound),
                }
            }
        }).await
    }
}

impl DavFileSystem for GmailDav {
    fn open<'a>(&'a self, path: &'a DavPath, _options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let content = self.get_content_bytes(path).await?;
            Ok(Box::new(GmailDavFile { 
                dav: self.clone(), 
                path: path.clone(), 
                content: Some(content), 
                size: None,
                pos: 0 
            }) as Box<dyn DavFile>)
        }.boxed()
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

            // Filter out tombstones
            let tombstones = self.tombstones.clone();
            let filtered_entries: Vec<_> = entries.into_iter().filter(|e| {
                let name = String::from_utf8_lossy(&e.name()).to_string();
                let full_child_path = if rel_path_str.is_empty() {
                    name
                } else {
                    format!("{}/{}", rel_path_str, name)
                };
                !tombstones.contains(&full_child_path)
            }).collect();

            let stream = futures::stream::iter(filtered_entries.into_iter().map(Ok));
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        }.boxed()
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

            if is_dir {
                Ok(Box::new(GmailDavMetaData::new(true, 0)) as Box<dyn DavMetaData>)
            } else if is_file {
                if parts[0] == "00_MOUNT_CHECK_OK" {
                    return Ok(Box::new(GmailDavMetaData::new(false, 2)) as Box<dyn DavMetaData>);
                }

                let (msg_display_name, file_name, is_attachment) = if parts.len() == 4 && parts[0] == "inbox" && parts[2] == "attachments" {
                    (parts[1], parts[3], true)
                } else if parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments" {
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
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            
            info!("create_dir: path={:?} parts={:?}", rel_path, parts);
            self.tombstones.remove(rel_path_str);

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

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            info!("remove_dir: path={:?} parts={:?}", rel_path, parts);

            if parts.len() == 2 && parts[0] == "search" {
                self.active_searches.remove(parts[1]);
                self.tombstones.insert(rel_path_str.to_string());
                Ok(())
            } else if (parts.len() == 2 && parts[0] == "inbox") || (parts.len() == 3 && parts[0] == "search") {
                let msg_display_name = if parts[0] == "inbox" { parts[1] } else { parts[2] };
                let msg_id = self.resolve_id(msg_display_name).ok_or(FsError::NotFound)?;
                self.client.trash_message(&msg_id).await.map_err(|e| {
                    error!("Trash failed: {}", e);
                    FsError::GeneralFailure
                })?;
                
                self.tombstones.insert(rel_path_str.to_string());
                // Clean up any child tombstones for this folder
                let child_prefix = format!("{}/", rel_path_str);
                self.tombstones.retain(|p| !p.starts_with(&child_prefix));
                Ok(())
            } else if (parts.len() == 3 && parts[0] == "inbox" && parts[2] == "attachments") ||
                      (parts.len() == 4 && parts[0] == "search" && parts[3] == "attachments") {
                self.tombstones.insert(rel_path_str.to_string());
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }.boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = path.as_rel_ospath();
            let rel_path_str = rel_path.to_str().unwrap_or("");
            let parts: Vec<&str> = rel_path_str.split('/').filter(|s| !s.is_empty()).collect();
            info!("remove_file: path={:?} parts={:?}", rel_path, parts);

            if (parts.len() == 3 && parts[0] == "inbox") || 
               (parts.len() == 4 && parts[0] == "search") ||
               (parts.len() == 4 && parts[0] == "inbox" && parts[2] == "attachments") ||
               (parts.len() == 5 && parts[0] == "search" && parts[3] == "attachments") {
                self.tombstones.insert(rel_path_str.to_string());
                Ok(())
            } else {
                Err(FsError::Forbidden)
            }
        }.boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, _to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel_path = from.as_rel_ospath();
            let parts: Vec<&str> = rel_path.to_str().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
            
            if parts.len() == 2 && parts[0] == "inbox" {
                let msg_id = self.resolve_id(parts[1]).ok_or(FsError::NotFound)?;
                self.client.archive_message(&msg_id).await.map_err(|_| FsError::GeneralFailure)?;
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
    dav: GmailDav,
    path: DavPath,
    content: Option<Bytes>,
    size: Option<u64>,
    pos: usize,
}

impl DavFile for GmailDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            if let Some(content) = &self.content {
                Ok(Box::new(GmailDavMetaData::new(false, content.len() as u64)) as Box<dyn DavMetaData>)
            } else if let Some(size) = self.size {
                Ok(Box::new(GmailDavMetaData::new(false, size)) as Box<dyn DavMetaData>)
            } else {
                let content = self.dav.get_content_bytes(&self.path).await?;
                let size = content.len() as u64;
                Ok(Box::new(GmailDavMetaData::new(false, size)) as Box<dyn DavMetaData>)
            }
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
            if self.content.is_none() {
                self.content = Some(self.dav.get_content_bytes(&self.path).await?);
            }
            let content = self.content.as_ref().unwrap();
            let start = self.pos;
            let end = std::cmp::min(start + count, content.len());
            let chunk = content.slice(start..end);
            self.pos = end;
            debug!("read_bytes: pos={} count={} returning={}", start, count, chunk.len());
            Ok(chunk)
        }.boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            if self.content.is_none() {
                self.content = Some(self.dav.get_content_bytes(&self.path).await?);
            }
            let content = self.content.as_ref().unwrap();
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
        }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}
