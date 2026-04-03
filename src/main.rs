mod gmail;
mod cache;
mod dav;

use std::sync::Arc;
use tokio::runtime::Runtime;
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
        let secret = google_gmail1::yup_oauth2::read_application_secret("credentials.json").await
            .map_err(|e| anyhow::anyhow!("Failed to read credentials.json: {}", e))?;

        let client = Arc::new(GmailClient::new(secret).await?);
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

        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let dav_handler = dav_handler.clone();

            tokio::task::spawn(async move {
                let service = service_fn(move |req| {
                    let dav_handler = dav_handler.clone();
                    async move {
                        Ok::<_, Infallible>(dav_handler.handle(req).await)
                    }
                });

                if let Err(err) = http1::Builder::new()
                    .serve_connection(io, service)
                    .await
                {
                    println!("Error serving connection: {:?}", err);
                }
            });
        }
    })
}
