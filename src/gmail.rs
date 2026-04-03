use google_gmail1::api::Message;
use google_gmail1::Gmail;
use google_gmail1::{yup_oauth2, common};
use google_gmail1::hyper_util::client::legacy::Client;
use google_gmail1::hyper_util::client::legacy::connect::HttpConnector;
use tracing::{debug, warn};
use serde_json::json;
use chrono::DateTime;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub attachment_id: String,
    pub size: u64,
}

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

    pub async fn list_inbox_messages(&self, max: u32) -> anyhow::Result<Vec<Message>> {
        debug!("Listing inbox messages (max {})", max);
        let (_, list) = self.hub.users().messages_list("me")
            .add_label_ids("INBOX")
            .max_results(max)
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

    pub async fn get_attachments_list(&self, id: &str) -> anyhow::Result<Vec<AttachmentMeta>> {
        let msg = self.get_message(id).await?;
        let mut list = Vec::new();
        if let Some(payload) = msg.payload {
            self.find_attachments(&payload, &mut list);
        }
        Ok(list)
    }

    fn find_attachments(&self, part: &google_gmail1::api::MessagePart, list: &mut Vec<AttachmentMeta>) {
        if let (Some(filename), Some(body)) = (&part.filename, &part.body) {
            if let Some(attachment_id) = &body.attachment_id {
                if !filename.is_empty() {
                    list.push(AttachmentMeta {
                        name: filename.clone(),
                        attachment_id: attachment_id.clone(),
                        size: body.size.unwrap_or(0) as u64,
                    });
                }
            }
        }
        if let Some(parts) = &part.parts {
            for p in parts {
                self.find_attachments(p, list);
            }
        }
    }

    pub async fn get_attachment_data(&self, message_id: &str, attachment_id: &str) -> anyhow::Result<Bytes> {
        debug!("Downloading attachment {} from message {}", attachment_id, message_id);
        let (_, body) = self.hub.users().messages_attachments_get("me", message_id, attachment_id)
            .add_scope(google_gmail1::api::Scope::Readonly.as_ref())
            .doit().await?;
        
        if let Some(data) = body.data {
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data)?;
            Ok(Bytes::from(decoded))
        } else {
            Err(anyhow::anyhow!("No data in attachment response"))
        }
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
        let mut date_str = "0000-00-00".to_string();
        let mut subject = "NoSubject".to_string();

        if let Some(payload) = &msg.payload {
            if let Some(headers) = &payload.headers {
                for h in headers {
                    match h.name.as_deref() {
                        Some("Date") => {
                            if let Some(val) = &h.value {
                                if let Ok(dt) = DateTime::parse_from_rfc2822(val) {
                                    date_str = dt.format("%Y-%m-%d").to_string();
                                }
                            }
                        }
                        Some("Subject") => {
                            if let Some(val) = &h.value {
                                let raw_subject = val.chars()
                                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                    .collect::<String>();
                                
                                let mut collapsed = String::new();
                                let mut last_was_underscore = false;
                                for c in raw_subject.chars() {
                                    if c == '_' {
                                        if !last_was_underscore {
                                            collapsed.push(c);
                                            last_was_underscore = true;
                                        }
                                    } else {
                                        collapsed.push(c);
                                        last_was_underscore = false;
                                    }
                                }
                                subject = collapsed.trim_matches('_').to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        subject.truncate(50);
        format!("{}_{}_{}", date_str, subject, msg.id.as_deref().unwrap_or("unknown"))
    }

    fn extract_body(&self, msg: &Message) -> String {
        if let Some(payload) = &msg.payload {
            return self.extract_part(payload);
        }
        String::new()
    }

    fn extract_part(&self, part: &google_gmail1::api::MessagePart) -> String {
        // Skip parts that are clearly attachments
        if part.filename.is_some() && !part.filename.as_ref().unwrap().is_empty() {
            return String::new();
        }

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
                let sub_mime = p.mime_type.as_deref().unwrap_or("");
                if sub_mime.starts_with("multipart/") {
                    let res = self.extract_part(p);
                    if !res.is_empty() { return res; }
                }
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
