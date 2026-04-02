use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InodeType {
    Folder,
    Message,
    Thread,
}

pub struct InodeStore {
    id_to_inode: DashMap<String, u64>,
    inode_to_id: DashMap<u64, (String, InodeType)>,
    next_inode: AtomicU64,
}

impl InodeStore {
    pub const ROOT: u64 = 1;
    pub const INBOX: u64 = 2;
    pub const LABELS: u64 = 3;
    pub const THREADS: u64 = 4;
    pub const ALL_MAIL: u64 = 5;
    pub const SEARCH: u64 = 6;

    pub fn new() -> Self {
        let id_to_inode = DashMap::new();
        let inode_to_id = DashMap::new();

        // Register static inodes
        id_to_inode.insert("inbox".to_string(), Self::INBOX);
        inode_to_id.insert(Self::INBOX, ("inbox".to_string(), InodeType::Folder));

        id_to_inode.insert("labels".to_string(), Self::LABELS);
        inode_to_id.insert(Self::LABELS, ("labels".to_string(), InodeType::Folder));

        id_to_inode.insert("threads".to_string(), Self::THREADS);
        inode_to_id.insert(Self::THREADS, ("threads".to_string(), InodeType::Folder));

        id_to_inode.insert("all_mail".to_string(), Self::ALL_MAIL);
        inode_to_id.insert(Self::ALL_MAIL, ("all_mail".to_string(), InodeType::Folder));

        id_to_inode.insert("search".to_string(), Self::SEARCH);
        inode_to_id.insert(Self::SEARCH, ("search".to_string(), InodeType::Folder));

        Self {
            id_to_inode,
            inode_to_id,
            next_inode: AtomicU64::new(100),
        }
    }

    pub fn get_or_create_inode(&self, id: String, r#type: InodeType) -> u64 {
        if let Some(inode) = self.id_to_inode.get(&id) {
            return *inode;
        }

        let inode = self.next_inode.fetch_add(1, Ordering::SeqCst);
        self.id_to_inode.insert(id.clone(), inode);
        self.inode_to_id.insert(inode, (id, r#type));
        inode
    }

    pub fn get_id(&self, inode: u64) -> Option<(String, InodeType)> {
        self.inode_to_id.get(&inode).map(|v| v.clone())
    }

    pub fn get_inode(&self, id: &str) -> Option<u64> {
        self.id_to_inode.get(id).map(|v| *v)
    }
}
