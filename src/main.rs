mod fs;
mod gmail;
mod inode;
mod cache;

use crate::fs::GmailFS;
use crate::gmail::GmailClient;
use crate::inode::InodeStore;
use crate::cache::MetadataCache;
use fuser::MountOption;
use std::sync::Arc;
use tokio::runtime::Runtime;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

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

    rt.block_on(async {
        // Load OAuth2 secret
        let secret = yup_oauth2::read_application_secret("credentials.json").await
            .map_err(|e| anyhow::anyhow!("Failed to read credentials.json: {}", e))?;

        let client = Arc::new(GmailClient::new(secret).await?);
        let inodes = Arc::new(InodeStore::new());
        let metadata_cache = Arc::new(MetadataCache::new());

        let fs = GmailFS {
            client,
            inodes,
            metadata_cache,
            handle,
        };

        let options = vec![
            MountOption::RO,
            MountOption::FSName("gmailfs".to_string()),
            MountOption::AutoUnmount,
        ];

        println!("Mounting GmailFS at {}", mountpoint);
        fuser::mount2(fs, mountpoint, &options)?;

        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}
