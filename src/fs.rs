use crate::cache::{MetadataCache, BodyCache};
use crate::gmail::GmailClient;
use crate::inode::{InodeStore, InodeType};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::ENOENT;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime::Handle;
use tracing::{debug, error, info};

const TTL: Duration = Duration::from_secs(1); // 1 second TTL for attributes
const PLACEHOLDER_SIZE: u64 = 64 * 1024; // 64KB placeholder

pub struct GmailFS {
    pub client: Arc<GmailClient>,
    pub inodes: Arc<InodeStore>,
    pub metadata_cache: Arc<MetadataCache>,
    pub body_cache: Arc<BodyCache>,
    pub handle: Handle,
}

impl GmailFS {
    fn root_attr(&self) -> FileAttr {
        FileAttr {
            ino: InodeStore::ROOT,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 501,
            gid: 20,
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn dir_attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 501,
            gid: 20,
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn file_attr(&self, ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: (size + 511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 501,
            gid: 20,
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn is_search_dir(&self, ino: u64) -> bool {
        if let Some((_, r#type)) = self.inodes.get_id(ino) {
            return r#type == InodeType::Folder && ino > 6;
        }
        false
    }
}

impl Filesystem for GmailFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy();
        debug!("lookup: parent={} name={}", parent, name_str);

        if parent == InodeStore::ROOT {
            match name_str.as_ref() {
                "inbox" => reply.entry(&TTL, &self.dir_attr(InodeStore::INBOX), 0),
                "labels" => reply.entry(&TTL, &self.dir_attr(InodeStore::LABELS), 0),
                "threads" => reply.entry(&TTL, &self.dir_attr(InodeStore::THREADS), 0),
                "all_mail" => reply.entry(&TTL, &self.dir_attr(InodeStore::ALL_MAIL), 0),
                "search" => reply.entry(&TTL, &self.dir_attr(InodeStore::SEARCH), 0),
                _ => reply.error(ENOENT),
            }
        } else if parent == InodeStore::INBOX || self.is_search_dir(parent) {
            // Looking for a message directory
            if let Some(ino) = self.inodes.get_inode(&name_str, InodeType::MessageDir) {
                reply.entry(&TTL, &self.dir_attr(ino), 0);
            } else {
                reply.error(ENOENT);
            }
        } else if parent == InodeStore::SEARCH {
            if let Some(ino) = self.inodes.get_inode(&name_str, InodeType::Folder) {
                reply.entry(&TTL, &self.dir_attr(ino), 0);
            } else {
                reply.error(ENOENT);
            }
        } else {
            // Could be a file inside a message directory
            if let Some((msg_id, InodeType::MessageDir)) = self.inodes.get_id(parent) {
                match name_str.as_ref() {
                    "body.md" => {
                        let ino = self.inodes.get_or_create_inode(msg_id, InodeType::BodyMd);
                        reply.entry(&TTL, &self.file_attr(ino, PLACEHOLDER_SIZE), 0);
                    }
                    "body.html" => {
                        let ino = self.inodes.get_or_create_inode(msg_id, InodeType::BodyHtml);
                        reply.entry(&TTL, &self.file_attr(ino, PLACEHOLDER_SIZE), 0);
                    }
                    "snippet.txt" => {
                        let ino = self.inodes.get_or_create_inode(msg_id, InodeType::SnippetTxt);
                        reply.entry(&TTL, &self.file_attr(ino, PLACEHOLDER_SIZE), 0);
                    }
                    "metadata.json" => {
                        let ino = self.inodes.get_or_create_inode(msg_id, InodeType::MetadataJson);
                        reply.entry(&TTL, &self.file_attr(ino, PLACEHOLDER_SIZE), 0);
                    }
                    "attachments" => {
                        let ino = self.inodes.get_or_create_inode(msg_id, InodeType::AttachmentsDir);
                        reply.entry(&TTL, &self.dir_attr(ino), 0);
                    }
                    _ => reply.error(ENOENT),
                }
            } else {
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);
        let handle = self.handle.clone();
        let metadata_cache = self.metadata_cache.clone();
        let body_cache = self.body_cache.clone();

        if let Some(attr) = handle.block_on(async { metadata_cache.get(ino).await }) {
            reply.attr(&TTL, &attr);
            return;
        }

        let attr = match ino {
            InodeStore::ROOT => self.root_attr(),
            InodeStore::INBOX
            | InodeStore::LABELS
            | InodeStore::THREADS
            | InodeStore::ALL_MAIL
            | InodeStore::SEARCH => self.dir_attr(ino),
            _ => {
                if let Some((id, r#type)) = self.inodes.get_id(ino) {
                    match r#type {
                        InodeType::Folder | InodeType::MessageDir | InodeType::AttachmentsDir => self.dir_attr(ino),
                        _ => {
                            let size = if let Some(body) = handle.block_on(async { body_cache.get(ino).await }) {
                                body.len() as u64
                            } else {
                                PLACEHOLDER_SIZE
                            };
                            self.file_attr(ino, size)
                        }
                    }
                } else {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        handle.block_on(async { metadata_cache.insert(ino, attr).await });
        reply.attr(&TTL, &attr);
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = name.to_string_lossy().to_string();
        info!("mkdir: parent={} name={}", parent, name_str);
        if parent == InodeStore::SEARCH {
            let ino = self.inodes.get_or_create_inode(name_str, InodeType::Folder);
            reply.entry(&TTL, &self.dir_attr(ino), 0);
        } else {
            reply.error(libc::EROFS);
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir: ino={} offset={}", ino, offset);
        if ino == InodeStore::ROOT {
            let entries = vec![
                (InodeStore::ROOT, FileType::Directory, "."),
                (InodeStore::ROOT, FileType::Directory, ".."),
                (InodeStore::INBOX, FileType::Directory, "inbox"),
                (InodeStore::LABELS, FileType::Directory, "labels"),
                (InodeStore::THREADS, FileType::Directory, "threads"),
                (InodeStore::ALL_MAIL, FileType::Directory, "all_mail"),
                (InodeStore::SEARCH, FileType::Directory, "search"),
            ];

            for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                    break;
                }
            }
            reply.ok();
        } else if ino == InodeStore::INBOX || self.is_search_dir(ino) {
            let handle = self.handle.clone();
            let client = self.client.clone();
            let inodes = self.inodes.clone();

            let messages = handle.block_on(async { 
                if ino == InodeStore::INBOX {
                    client.list_inbox_messages().await 
                } else {
                    let (query, _) = inodes.get_id(ino).unwrap();
                    client.search_messages(&query).await
                }
            });

            match messages {
                Ok(msgs) => {
                    let mut entries = vec![
                        (ino, FileType::Directory, ".".to_string()),
                        (if ino == InodeStore::INBOX { InodeStore::ROOT } else { InodeStore::SEARCH }, FileType::Directory, "..".to_string()),
                    ];

                    for msg in msgs {
                        // In a real impl, we'd fetch headers here to get the real display name.
                        // For efficiency in listing, we'll use the ID-based display name from get_display_name
                        // but note that get_display_name needs a full message object.
                        // For now we use the ID as the directory name.
                        let msg_id = msg.id.unwrap_or_default();
                        entries.push((0, FileType::Directory, msg_id));
                    }

                    for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                        let mut entry_ino = entry.0;
                        if entry_ino == 0 {
                            entry_ino = inodes.get_or_create_inode(entry.2.clone(), InodeType::MessageDir);
                        }
                        if reply.add(entry_ino, (i + 1) as i64, entry.1, &entry.2) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => {
                    error!("readdir failed: {}", e);
                    reply.error(libc::EIO);
                }
            }
        } else if ino == InodeStore::SEARCH {
            if offset == 0 {
                let _ = reply.add(ino, 1, FileType::Directory, ".");
                let _ = reply.add(InodeStore::ROOT, 2, FileType::Directory, "..");
            }
            reply.ok();
        } else {
            // It's likely a MessageDir
            if let Some((msg_id, InodeType::MessageDir)) = self.inodes.get_id(ino) {
                let entries = vec![
                    (ino, FileType::Directory, "."),
                    (InodeStore::INBOX, FileType::Directory, ".."), // Simplified ..
                    (self.inodes.get_or_create_inode(msg_id.clone(), InodeType::BodyMd), FileType::RegularFile, "body.md"),
                    (self.inodes.get_or_create_inode(msg_id.clone(), InodeType::BodyHtml), FileType::RegularFile, "body.html"),
                    (self.inodes.get_or_create_inode(msg_id.clone(), InodeType::SnippetTxt), FileType::RegularFile, "snippet.txt"),
                    (self.inodes.get_or_create_inode(msg_id.clone(), InodeType::MetadataJson), FileType::RegularFile, "metadata.json"),
                    (self.inodes.get_or_create_inode(msg_id.clone(), InodeType::AttachmentsDir), FileType::Directory, "attachments"),
                ];

                for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                        break;
                    }
                }
                reply.ok();
            } else {
                reply.error(ENOENT);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if let Some((id, r#type)) = self.inodes.get_id(ino) {
            let handle = self.handle.clone();
            let client = self.client.clone();
            let body_cache = self.body_cache.clone();

            let content_res = if let Some(cached) = handle.block_on(async { body_cache.get(ino).await }) {
                Ok(cached)
            } else {
                let res = handle.block_on(async {
                    match r#type {
                        InodeType::BodyMd => client.get_message_markdown(&id).await,
                        InodeType::BodyHtml => client.get_message_html(&id).await,
                        InodeType::SnippetTxt => client.get_message_snippet(&id).await,
                        InodeType::MetadataJson => client.get_message_metadata(&id).await,
                        _ => Err(anyhow::anyhow!("Not a readable file type")),
                    }
                });
                if let Ok(content) = &res {
                    handle.block_on(async { body_cache.insert(ino, content.clone()).await });
                }
                res
            };

            match content_res {
                Ok(content) => {
                    let bytes = content.as_bytes();
                    if offset >= bytes.len() as i64 {
                        reply.data(&[]);
                    } else {
                        let end = std::cmp::min(offset as usize + size as usize, bytes.len());
                        reply.data(&bytes[offset as usize..end]);
                    }
                }
                Err(e) => {
                    error!("read failed for ino={}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            reply.error(ENOENT);
        }
    }
}
