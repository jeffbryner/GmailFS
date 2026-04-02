use google_gmail1::api::Message;
use google_gmail1::Gmail;
use yup_oauth2::{self as oauth2};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};

pub struct GmailClient {
    hub: Gmail<HttpsConnector<HttpConnector>>,
}

impl GmailClient {
    pub async fn new(secret: oauth2::ApplicationSecret) -> anyhow::Result<Self> {
        let auth = oauth2::InstalledFlowAuthenticator::builder(
            secret,
            oauth2::InstalledFlowReturnMethod::HTTPRedirect,
        )
        .persist_tokens_to_disk("gmailfs_tokens.json")
        .build()
        .await?;

        let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(
            HttpsConnectorBuilder::new()
                .with_native_roots()?
                .https_only()
                .enable_http1()
                .build(),
        );

        let hub = Gmail::new(client, auth);
        Ok(Self { hub })
    }

    pub async fn list_inbox_messages(&self) -> anyhow::Result<Vec<Message>> {
        let (_, list) = self.hub.users().messages_list("me")
            .add_label_ids("INBOX")
            .doit().await?;
        
        Ok(list.messages.unwrap_or_default())
    }

    pub async fn get_message(&self, id: &str) -> anyhow::Result<Message> {
        let (_, msg) = self.hub.users().messages_get("me", id)
            .format("full")
            .doit().await?;
        Ok(msg)
    }

    pub async fn get_message_markdown(&self, id: &str) -> anyhow::Result<String> {
        let msg = self.get_message(id).await?;
        let body = self.extract_body(&msg);
        Ok(html2md::parse_html(&body))
    }

    fn extract_body(&self, msg: &Message) -> String {
        if let Some(payload) = &msg.payload {
            return self.extract_part(payload);
        }
        msg.snippet.clone().unwrap_or_default()
    }

    fn extract_part(&self, part: &google_gmail1::api::MessagePart) -> String {
        if let Some(body) = &part.body {
            if let Some(data) = &body.data {
                if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, data) {
                    return String::from_utf8_lossy(&decoded).to_string();
                }
            }
        }

        if let Some(parts) = &part.parts {
            // Prefer HTML, then plain text
            for p in parts {
                if p.mime_type.as_deref() == Some("text/html") {
                    return self.extract_part(p);
                }
            }
            for p in parts {
                if p.mime_type.as_deref() == Some("text/plain") {
                    return self.extract_part(p);
                }
            }
            for p in parts {
                let body = self.extract_part(p);
                if !body.is_empty() {
                    return body;
                }
            }
        }

        String::new()
    }

    pub async fn search_messages(&self, query: &str) -> anyhow::Result<Vec<Message>> {
        let (_, list) = self.hub.users().messages_list("me")
            .q(query)
            .doit().await?;
        
        Ok(list.messages.unwrap_or_default())
    }
}
