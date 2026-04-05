mod gmail;
mod cache;
mod dav;

use std::sync::Arc;
use tokio::runtime::Runtime;
use tracing::debug;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use dav_server::DavHandler;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use crate::gmail::GmailClient;
use crate::cache::BodyCache;
use crate::dav::GmailDav;
use hyper::service::service_fn;
use std::convert::Infallible;
use google_gmail1::hyper_util::client::legacy::Client;
use google_gmail1::hyper_rustls::HttpsConnectorBuilder;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};

fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,gmailfs=debug"));
    
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    let rt = Runtime::new()?;
    rt.block_on(async {
        // Outgoing connector with HTTP/2 support for connection multiplexing
        let connector = HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_only()
            .enable_http1()
            .enable_http2()
            .build();
        
        let executor = google_gmail1::hyper_util::rt::TokioExecutor::new();
        
        // Shared clients with limited connection pools
        let auth_client = Client::builder(executor.clone())
            .pool_max_idle_per_host(5)
            .build(connector.clone());
            
        let hub_client = Client::builder(executor)
            .pool_max_idle_per_host(10)
            .build(connector);

        let secret = google_gmail1::yup_oauth2::read_application_secret("credentials.json").await
            .map_err(|e| anyhow::anyhow!("Failed to read credentials.json: {}", e))?;

        let client = Arc::new(GmailClient::new(
            secret, 
            hub_client, 
            google_gmail1::yup_oauth2::client::CustomHyperClientBuilder::from(auth_client)
        ).await?);
        
        let body_cache = Arc::new(BodyCache::new());
        
        let dav_fs = GmailDav::new(client, body_cache);
        let dav_handler = DavHandler::builder()
            .filesystem(Box::new(dav_fs))
            .locksystem(dav_server::memls::MemLs::new())
            .build_handler();

        let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
        let listener = TcpListener::bind(addr).await?;
        println!("GmailFS WebDAV server listening on http://{}", addr);
        println!("To mount on macOS: mount_webdav -i http://localhost:8080 /tmp/gmail");

        // Limit concurrent incoming WebDAV connections to prevent FD exhaustion
        let conn_semaphore = Arc::new(Semaphore::new(50));

        loop {
            let permit = conn_semaphore.clone().acquire_owned().await?;
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let dav_handler = dav_handler.clone();

            tokio::task::spawn(async move {
                let _permit = permit; // Hold permit until connection closes
                let service = service_fn(move |req| {
                    let dav_handler = dav_handler.clone();
                    async move {
                        Ok::<_, Infallible>(dav_handler.handle(req).await)
                    }
                });

                // Add a timeout to connection serving to prevent hung sockets
                let conn = http1::Builder::new()
                    .serve_connection(io, service);
                
                if let Err(err) = timeout(Duration::from_secs(30), conn).await {
                    debug!("Connection timed out or error: {:?}", err);
                }
            });
        }
    })
}
