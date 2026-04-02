use google_gmail1::api::Message;
use google_gmail1::Gmail;
use google_gmail1::{yup_oauth2, common};
use google_gmail1::hyper_util::client::legacy::Client;
use google_gmail1::hyper_util::client::legacy::connect::HttpConnector;
use tracing::{debug, warn};
use serde_json::json;

pub struct GmailClient {
    hub: Gmail<google_gmail1::hyper_rustls::HttpsConnector<HttpConnector>>,
}

impl GmailClient {
    pub async fn new(secret: yup_oauth2::ApplicationSecret) -> anyhow::Result<Self> {
        let connector = google_gmail1::hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_only()
            .enable_http1()
            .build();
        
        let executor = google_gmail1::hyper_util::rt::TokioExecutor::new();
        
        let auth_client = Client::builder(executor.clone()).build(connector.clone());
        let hub_client: common::Client<google_gmail1::hyper_rustls::HttpsConnector<HttpConnector>> = 
            Client::builder(executor).build(connector);

        let auth = yup_oauth2::InstalledFlowAuthenticator::with_client(
            secret,
            yup_oauth2::InstalledFlowReturnMethod::HTTPRedirect,
            yup_oauth2::client::CustomHyperClientBuilder::from(auth_client),
        )
        .persist_tokens_to_disk("gmailfs_tokens.json")
        .build()
        .await?;

        let hub = Gmail::new(hub_client, auth);
        Ok(Self { hub })
    }

    pub async fn list_inbox_messages(&self) -> anyhow::Result<Vec<Message>> {
        debug!("Listing inbox messages");
        let (_, list) = self.hub.users().messages_list("me")
            .add_label_ids("INBOX")
            .add_scope(google_gmail1::api::Scope::Readonly.as_ref())
            .doit().await?;
        
        Ok(list.messages.unwrap_or_default())
    }

    pub async fn get_message(&self, id: &str) -> anyhow::Result<Message> {
        debug!("Fetching message {}", id);
        let (_, msg) = self.hub.users().messages_get("me", id)
            .format("full")
            .add_scope(google_gmail1::api::Scope::Readonly.as_ref())
            .doit().await?;
        Ok(msg)
    }

    pub async fn get_message_markdown(&self, id: &str) -> anyhow::Result<String> {
        let msg = self.get_message(id).await?;
        let body = self.extract_body(&msg);
        
        let mut result = if body.is_empty() {
            msg.snippet.clone().unwrap_or_default()
        } else {
            let cleaned = ammonia::clean(&body);
            html2md::parse_html(&cleaned)
        };

        if !result.ends_with('\n') {
            result.push('\n');
        }
        Ok(result)
    }

    pub async fn get_message_html(&self, id: &str) -> anyhow::Result<String> {
        let msg = self.get_message(id).await?;
        Ok(self.extract_body(&msg))
    }

    pub async fn get_message_snippet(&self, id: &str) -> anyhow::Result<String> {
        let msg = self.get_message(id).await?;
        Ok(msg.snippet.unwrap_or_default())
    }

    pub async fn get_message_metadata(&self, id: &str) -> anyhow::Result<String> {
        let msg = self.get_message(id).await?;
        let mut metadata = json!({
            "id": msg.id,
            "threadId": msg.thread_id,
            "snippet": msg.snippet,
            "internalDate": msg.internal_date,
        });

        if let Some(payload) = msg.payload {
            if let Some(headers) = payload.headers {
                let mut h_map = json!({});
                for h in headers {
                    if let (Some(name), Some(value)) = (h.name, h.value) {
                        h_map[name] = json!(value);
                    }
                }
                metadata["headers"] = h_map;
            }
        }

        Ok(serde_json::to_string_pretty(&metadata)?)
    }

    pub fn get_display_name(&self, msg: &Message) -> String {
        let mut date = "UnknownDate".to_string();
        let mut subject = "NoSubject".to_string();

        if let Some(payload) = &msg.payload {
            if let Some(headers) = &payload.headers {
                for h in headers {
                    match h.name.as_deref() {
                        Some("Date") => {
                            if let Some(val) = &h.value {
                                // Simplify date for filename: Tue, 1 Apr 2026 -> 2026-04-01
                                date = val.clone(); 
                            }
                        }
                        Some("Subject") => {
                            if let Some(val) = &h.value {
                                subject = val.chars()
                                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                    .collect::<String>();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Limit subject length
        subject.truncate(50);
        format!("{}_{}", msg.id.as_deref().unwrap_or("unknown"), subject)
    }

    fn extract_body(&self, msg: &Message) -> String {
        if let Some(payload) = &msg.payload {
            return self.extract_part(payload);
        }
        String::new()
    }

    fn extract_part(&self, part: &google_gmail1::api::MessagePart) -> String {
        let mime_type = part.mime_type.as_deref().unwrap_or("text/plain");
        
        if let Some(body) = &part.body {
            if let Some(data) = &body.data {
                if !data.is_empty() {
                    if data.starts_with(b"<") || data.contains(&b'\n') || data.contains(&b' ') {
                        return String::from_utf8_lossy(data).to_string();
                    }

                    use base64::Engine as _;
                    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
                    match engine.decode(data) {
                        Ok(decoded) => {
                            let content = String::from_utf8_lossy(&decoded).to_string();
                            if !content.is_empty() { return content; }
                        }
                        Err(_) => {
                            return String::from_utf8_lossy(data).to_string();
                        }
                    }
                }
            }
        }

        if let Some(parts) = &part.parts {
            for p in parts {
                if p.mime_type.as_deref() == Some("text/html") {
                    let res = self.extract_part(p);
                    if !res.is_empty() { return res; }
                }
            }
            for p in parts {
                if p.mime_type.as_deref() == Some("text/plain") {
                    let res = self.extract_part(p);
                    if !res.is_empty() { return res; }
                }
            }
            for p in parts {
                let res = self.extract_part(p);
                if !res.is_empty() { return res; }
            }
        }

        String::new()
    }

    pub async fn search_messages(&self, query: &str) -> anyhow::Result<Vec<Message>> {
        let (_, list) = self.hub.users().messages_list("me")
            .q(query)
            .add_scope(google_gmail1::api::Scope::Readonly.as_ref())
            .doit().await?;
        
        Ok(list.messages.unwrap_or_default())
    }
}
