use crate::cache::MetadataCache;
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

const TTL: Duration = Duration::from_secs(1); // 1 second TTL for attributes

pub struct GmailFS {
    pub client: Arc<GmailClient>,
    pub inodes: Arc<InodeStore>,
    pub metadata_cache: Arc<MetadataCache>,
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
            blocks: 1,
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
        if let Some((_id, r#type)) = self.inodes.get_id(ino) {
            return r#type == InodeType::Folder && ino > 6;
        }
        false
    }
}

impl Filesystem for GmailFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy();

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
            if let Some(ino) = self.inodes.get_inode(&name_str) {
                reply.entry(&TTL, &self.file_attr(ino, 0), 0);
            } else {
                reply.error(ENOENT);
            }
        } else if parent == InodeStore::SEARCH {
            if let Some(ino) = self.inodes.get_inode(&name_str) {
                reply.entry(&TTL, &self.dir_attr(ino), 0);
            } else {
                reply.error(ENOENT);
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let handle = self.handle.clone();
        let cache = self.metadata_cache.clone();

        if let Some(attr) = handle.block_on(async { cache.get(ino).await }) {
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
                if let Some((_, r#type)) = self.inodes.get_id(ino) {
                    match r#type {
                        InodeType::Folder => self.dir_attr(ino),
                        InodeType::Message | InodeType::Thread => self.file_attr(ino, 0),
                    }
                } else {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        handle.block_on(async { cache.insert(ino, attr).await });
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
        if parent == InodeStore::SEARCH {
            let name_str = name.to_string_lossy().to_string();
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
        } else if ino == InodeStore::INBOX {
            let handle = self.handle.clone();
            let client = self.client.clone();
            let inodes = self.inodes.clone();

            let messages = handle.block_on(async { client.list_inbox_messages().await });

            match messages {
                Ok(msgs) => {
                    let mut entries = vec![
                        (ino, FileType::Directory, ".".to_string()),
                        (InodeStore::ROOT, FileType::Directory, "..".to_string()),
                    ];

                    for msg in msgs {
                        let msg_id = msg.id.unwrap_or_default();
                        entries.push((0, FileType::RegularFile, msg_id));
                    }

                    for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                        let mut entry_ino = entry.0;
                        if entry_ino == 0 {
                            entry_ino = inodes.get_or_create_inode(entry.2.clone(), InodeType::Message);
                        }
                        if reply.add(entry_ino, (i + 1) as i64, entry.1, &entry.2) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(_) => reply.error(libc::EIO),
            }
        } else if ino == InodeStore::SEARCH {
            if offset == 0 {
                reply.add(ino, 1, FileType::Directory, ".");
                reply.add(InodeStore::ROOT, 2, FileType::Directory, "..".to_string());
            }
            reply.ok();
        } else if self.is_search_dir(ino) {
            if let Some((query, _)) = self.inodes.get_id(ino) {
                let handle = self.handle.clone();
                let client = self.client.clone();
                let inodes = self.inodes.clone();

                let messages = handle.block_on(async { client.search_messages(&query).await });

                match messages {
                    Ok(msgs) => {
                        let mut entries = vec![
                            (ino, FileType::Directory, ".".to_string()),
                            (InodeStore::SEARCH, FileType::Directory, "..".to_string()),
                        ];

                        for msg in msgs {
                            let msg_id = msg.id.unwrap_or_default();
                            entries.push((0, FileType::RegularFile, msg_id));
                        }

                        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                            let mut entry_ino = entry.0;
                            if entry_ino == 0 {
                                entry_ino = inodes.get_or_create_inode(entry.2.clone(), InodeType::Message);
                            }
                            if reply.add(entry_ino, (i + 1) as i64, entry.1, &entry.2) {
                                break;
                            }
                        }
                        reply.ok();
                    }
                    Err(_) => reply.error(libc::EIO),
                }
            } else {
                reply.error(ENOENT);
            }
        } else {
            reply.error(ENOENT);
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
            if r#type == InodeType::Message {
                let handle = self.handle.clone();
                let client = self.client.clone();

                let message = handle.block_on(async { client.get_message_markdown(&id).await });

                match message {
                    Ok(markdown) => {
                        let content = markdown.as_bytes();
                        let end = std::cmp::min(offset as usize + size as usize, content.len());
                        if offset as usize >= content.len() {
                            reply.data(&[]);
                        } else {
                            reply.data(&content[offset as usize..end]);
                        }
                    }
                    Err(_) => reply.error(libc::EIO),
                }
            } else {
                reply.error(libc::EISDIR);
            }
        } else {
            reply.error(ENOENT);
        }
    }
}
