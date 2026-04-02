mod fs;
mod gmail;
mod inode;
mod cache;

use crate::fs::GmailFS;
use crate::gmail::GmailClient;
use crate::inode::InodeStore;
use crate::cache::{MetadataCache, BodyCache};
use fuser::MountOption;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> anyhow::Result<()> {
    // Configure logging: default to INFO, but DEBUG for our crate
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,gmailfs=debug"));
    
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    // rustls 0.23+ requires a default CryptoProvider to be set if multiple providers are enabled.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        println!("Usage: {} <mountpoint>", args[0]);
        return Ok(());
    }
    let mountpoint = &args[1];

    let rt = Runtime::new()?;
    let handle = rt.handle().clone();

    // 1. Perform async initialization
    let (client, inodes, metadata_cache, body_cache) = rt.block_on(async {
        let secret = google_gmail1::yup_oauth2::read_application_secret("credentials.json").await
            .map_err(|e| anyhow::anyhow!("Failed to read credentials.json: {}", e))?;

        let client = Arc::new(GmailClient::new(secret).await?);
        let inodes = Arc::new(InodeStore::new());
        let metadata_cache = Arc::new(MetadataCache::new());
        let body_cache = Arc::new(BodyCache::new());
        
        Ok::<(Arc<GmailClient>, Arc<InodeStore>, Arc<MetadataCache>, Arc<BodyCache>), anyhow::Error>((client, inodes, metadata_cache, body_cache))
    })?;

    // 2. Prepare the filesystem
    let fs = GmailFS {
        client,
        inodes,
        metadata_cache,
        body_cache,
        handle,
    };

    let options = vec![
        MountOption::RO,
        MountOption::FSName("gmailfs".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowOther,
    ];

    // 3. Mount on the main thread (NOT a runtime thread)
    println!("Mounting GmailFS at {}", mountpoint);
    fuser::mount2(fs, mountpoint, &options)?;

    Ok(())
}
