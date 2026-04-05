# GmailFS

**GmailFS** is a Rust-based WebDAV daemon that maps your Gmail account to a local directory structure. It is designed specifically for **Agentic Workflows**, allowing LLM agents and terminal users to interact with email using standard filesystem tools like `ls`, `grep`, `cat`, and `rm`.

By presenting email as a filesystem, GmailFS eliminates the need for agents to handle complex REST APIs, OAuth2 flows, or MIME parsing.

## Features

- **Markdown-First**: Bodies are automatically converted to clean Markdown (`body.md`) for efficient LLM processing.
- **Descriptive Naming**: Folders are named `[YYYY-MM-DD]_[Subject]_[ID]` for easy identification and tab-completion.
- **Magic Search**: Trigger live Gmail queries by simply creating a directory (e.g., `mkdir search/"from:google"`).
- **Outbox Support**: Send emails by writing a single text file to the `/outbox` folder.
- **Live Attachments**: Access email attachments as files in the `attachments/` sub-folder.
- **Native Operations**: `rm -rf` trashes emails, and `mv` archives them.
- **Driverless**: Runs as a userspace WebDAV server; no kernel extensions (FUSE) required.

---

## 1. Setup & Installation

### Prerequisites
- [Rust](https://www.rust-lang.org/tools/install) (latest stable)
- A Google Cloud Project

### Step 1: Google Cloud Configuration
1. Go to the [Google Cloud Console](https://console.cloud.google.com/).
2. Create a new project or select an existing one.
3. Enable the **Gmail API**.
4. Configure the **OAuth Consent Screen**:
   - Set User Type to **External** (or Internal if using Google Workspace).
   - Add the following scopes:
     - `https://www.googleapis.com/auth/gmail.modify`
     - `https://www.googleapis.com/auth/gmail.send`
5. Create **OAuth 2.0 Client IDs**:
   - Application type: **Desktop App**.
   - Download the JSON credentials file and rename it to `credentials.json`.
   - Place `credentials.json` in the root of this project.

### Step 2: Running the Daemon
```bash
cargo run
```
On the first run, a browser window will open asking you to authorize the application. Once complete, a `gmailfs_tokens.json` file will be created to persist your session.

### Step 3: Mounting on macOS
```bash
mkdir -p /tmp/gmail
mount_webdav -i http://localhost:8080 /tmp/gmail
cd /tmp/gmail
```

---

## 2. Usage Examples

### Listing the Inbox
```bash
ls -F inbox/
# 2026-04-01_Project_Update_19d4bbf73df4277b/
# 2026-04-02_Meeting_Notes_19d52fc05f9d2f54/
```

### Reading an Email
```bash
cat inbox/[email_dir]/body.md
```

### Searching Gmail
To find all emails from "Anthropic":
```bash
mkdir search/"from:Anthropic"
ls -F search/"from:Anthropic"
```

### Sending an Email
Simply `cat` a file into the `/outbox` directory. The first few lines should contain headers, followed by an empty line and the body.
```bash
cat <<EOF > outbox/new_message.md
To: someone@example.com
Subject: Hello from GmailFS

This is the body of the email.
It supports Markdown formatting.
EOF
```

### Downloading Attachments
```bash
ls inbox/[email_dir]/attachments/
cp inbox/[email_dir]/attachments/report.pdf ~/Desktop/
```

### Trashing & Archiving
- **Trash**: `rm -rf inbox/[email_dir]`
- **Archive**: `mv inbox/[email_dir] /tmp/` (Moving an email out of the inbox removes the `INBOX` label)

---

## 3. Troubleshooting

### "Too many open files" error
On macOS, the default limit for open files is often low. If you encounter this error, increase the limit in your terminal before running the daemon:
```bash
ulimit -n 4096
```

---

## 4. Directory Structure
```text
/tmp/gmail/
├── inbox/                  # Your Inbox
├── unread/                 # Unread messages only
├── outbox/                 # Write files here to send emails
├── search/                 # Register queries via mkdir
│   └── [Query]/
│       └── [EmailDir]/
│           ├── body.md
│           ├── body.html
│           ├── metadata.json
│           └── attachments/
└── 00_MOUNT_CHECK_OK       # Verification file
```

## License
Mozilla Public License Version 2.0
