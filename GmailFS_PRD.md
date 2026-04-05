# PRD: GmailFS – A WebDAV Filesystem for Agentic Workflows

## 1. Vision & Executive Summary
**GmailFS** is a Rust-based WebDAV daemon that maps a user's Gmail account to a local directory structure. 
* **The Problem:** Large Language Model (LLM) agents struggle with complex REST APIs, OAuth2 flows, and MIME parsing.
* **The Solution:** Presenting email as a filesystem via a local WebDAV server allows agents to use familiar tools (`ls`, `grep`, `cat`, `rm`) to manage communication without requiring kernel-level drivers.
* **Core Tech:** Rust, `dav-server` (WebDAV), `google-gmail1` (API), `tokio` (Async), `moka` (Caching), `hyper` (HTTP).

---

## 2. Target Directory Structure
The agent interacts with the following hierarchy:

```text
/mnt/gmail/
├── inbox/                  # The last 20 messages (customizable)
│   └── [YYYY-MM-DD]_[Sanitized_Subject]_[MsgID]/
│       ├── body.md         # HTML converted to Markdown (Primary)
│       ├── body.html       # Raw HTML (Fallback)
│       ├── snippet.txt     # Plain text preview
│       ├── metadata.json   # JSON headers (Date, From, To, Snippet)
│       └── attachments/    # Nested directory for attachments
├── search/
│   └── [LiveQuery]/        # Results of mkdir [Query] as symlink-like folders
└── unread/                 # Same as inbox but only unread messages
```

---

## 3. Key Features

### 3.1 Magic Search Implementation
* **Virtual Query Nodes:** When the agent creates a directory (e.g., `mkdir search/"from:internal.com"`), the daemon registers that query string.
* **Lazy Population:** The contents of a search folder are only fetched via the Gmail API when `read_dir` is called. Results follow the same `[Date]_[Subject]` directory pattern used in the inbox.

### 3.2 Message Presentation (MIME to Markdown)
* **Conversion Engine:** Uses `ammonia` for HTML sanitization and `html2md` to provide a clean `body.md`.
* **Streaming Access:** WebDAV handles dynamic file sizes gracefully, allowing the daemon to calculate exact sizes on-the-fly.
* **Terminal Friendly:** All filenames are sanitized (spaces replaced with underscores, multiples collapsed) for perfect tab completion.

### 3.3 Caching Mechanism
* **Body Cache:** Uses `moka` to persist converted `.md` and `.json` files for 300 seconds to save API quota and latency on repeated reads.
* **Path Mapping:** A session-based `DashMap` translates descriptive filesystem paths back to Gmail's internal Hex IDs.

---

## 4. Technical Constraints & Logic

### 4.1 Path-to-ID Mapping
* Since WebDAV is path-based, the daemon maintains a bidirectional mapping between the descriptive directory names and the Gmail Message IDs.
* **Logic:** `[YYYY-MM-DD]_[Subject]_[ID]` ensures that filenames remain unique and stable during a session.

### 4.2 Driverless Architecture
* By using WebDAV instead of FUSE, the daemon runs as a standard userspace HTTP server.
* **Mounting:** Uses macOS's native `mount_webdav` utility, avoiding the need for kernel extensions or MacFUSE.

### 4.3 Permissions
* `GET` and `PROPFIND` map to `read` permissions.
* `MKCOL` (mkdir) inside `/search` registers new queries.
* `DELETE` maps to `messages.trash`.

---

## 5. Implementation Roadmap (Iterative Development)

### Milestone 1: Core Connectivity (Completed)
* Setup WebDAV server boilerplate using `dav-server`.
* Implement OAuth2 login and basic message listing.

### Milestone 2: The Presentation Layer (Completed)
* Implement `body.md` conversion with `ammonia` sanitization.
* Implement Descriptive Naming (`YYYY-MM-DD_Subject`).
* Implement Magic Search via `mkdir`.

### Milestone 3: Advanced Operations (Completed)
* Support for live attachment downloads.
* Support for trashing/moving emails via `rm -rf <directory representing the individual email>`.

### Milestone 4: Agent friendly email sending
Optimized for speed. An agent just writes a single .md file to a special directory, and the daemon sends it on close.
This minimizes the number of filesystem operations an agent needs to perform, making it the most "agent-friendly."
To an agent, sending email looks like a single, atomic operation—the simplest possible way to interact with an external system. 

  Here is exactly how an agent (like gemini-cli or a Python script) would send an email using GmailFS:

  1. The Command
  The agent simply uses a single cat command (or its equivalent write_file tool):
```
    1 cat <<EOF > /tmp/gmail/outbox/status_report.md
    2 To: team@example.com
    3 Subject: Weekly Progress Update
    4
    5 ## Accomplishments
    6 - Implemented WebDAV backend.
    7 - Optimized attachment downloads.
    8 - Added unread folder support.
    9
   10 This update was sent automatically by the GmailFS agent.
   11 EOF
```


---

## 6. Usage for Agents
1. **Start Server:** `cargo run` (Listens on http://localhost:8080)
2. **Mount:** `mount_webdav -i http://localhost:8080 /tmp/gmail`
3. **Verify:** Check for the `00_MOUNT_CHECK_OK` file in the root.
