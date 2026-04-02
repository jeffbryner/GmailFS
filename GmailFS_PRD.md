This **Product Requirements Document (PRD)** is designed to be fed directly into a coding assistant like `gemini-cli`. It outlines the technical architecture, file mapping logic, and Rust-specific implementation details for **GmailFS**.

---

# PRD: GmailFS – A FUSE Filesystem for Agentic Workflows

## 1. Vision & Executive Summary
**GmailFS** is a Rust-based FUSE daemon that maps a user's Gmail account to a local directory structure. 
* **The Problem:** Large Language Model (LLM) agents struggle with complex REST APIs, OAuth2 flows, and MIME parsing.
* **The Solution:** Presenting email as a filesystem allows agents to use familiar tools (`ls`, `grep`, `cat`, `rm`) to manage communication.
* **Core Tech:** Rust, `fuser` (FUSE), `google-gmail1` (API), `tokio` (Async), `moka` (Caching).

---

## 2. Target Directory Structure
The agent should interact with the following hierarchy:

```text
/mnt/gmail/
├── inbox/                  # Symlinks to messages with 'INBOX' label
├── labels/
│   └── [LabelName]/        # Folders representing Gmail labels
├── threads/
│   └── [ThreadID]/         # Directory containing all messages in a thread
├── all_mail/               # The "Source of Truth" (all message objects)
│   └── [MsgID]/
│       ├── body.md         # HTML converted to Markdown (Primary)
│       ├── body.html       # Raw HTML (Fallback)
│       ├── metadata.json   # JSON headers (Date, From, To, Snippet)
│       └── attachments/    # Nested directory for attachments
└── search/
    ├── .query              # Write-only file to trigger searches
    └── [QueryHash]/        # Results of the last X queries as symlinks
```

---

## 3. Key Functional Requirements

### 3.1 Magic Search Implementation
* **Virtual Query Nodes:** When the agent writes a Gmail query (e.g., `from:internal.com`) to `search/.query`, the daemon shall create a folder named after the query or its hash.
* **Lazy Population:** The contents of a search folder are only fetched when `readdir` is called. Results are symlinks to the `all_mail/[MsgID]` directory.

### 3.2 Message Presentation (MIME to Markdown)
* **Conversion Engine:** Use a Rust crate (e.g., `html2md`) to strip HTML/CSS and provide a clean `body.md`.
* **Lazy Loading:** Do not fetch or convert the message body until the `read` syscall is invoked on `body.md`.
* **Attachment Handling:** Map Gmail Attachment IDs to virtual files. Trigger download only on access.

### 3.3 Caching Mechanism
* **Metadata Cache:** Use `moka` to cache file attributes (`stat`) and directory listings for 60 seconds.
* **Body Cache:** Persist converted `.md` files to a local temporary directory to save API quota and latency on repeated reads.
* **Invalidation:** Listen for new mail events (via Gmail Pub/Sub if possible) to invalidate the `/inbox` cache.

---

## 4. Technical Constraints & Logic

### 4.1 Inode Management
* Gmail IDs (Strings) must be mapped to unique `u64` Inodes. 
* **Logic:** Implement a persistent or deterministic mapping (e.g., a hash map backed by a local SQLite/Sled DB/DuckDB) so Inodes remain consistent across restarts.

### 4.2 Rate Limiting (The "Backoff" Layer)
* The daemon must implement a request-throttling queue. If an agent runs `find /mnt/gmail -name "*.pdf"`, the daemon must prevent a 429 "Too Many Requests" error by batching calls or slowing down the FUSE response.

### 4.3 Permissions
* `GET` requests map to `read` permissions.
* `DELETE` syscalls map to `messages.trash` or `messages.batchModify`.
* `WRITE` to a `.send` file (future scope) should trigger a `messages.send`.

---

## 5. Implementation Roadmap (Iterative Development)

### Milestone 1: Read-Only Skeleton
* Setup `fuser` boilerplate in Rust.
* Implement OAuth2 login and basic `readdir` for the Inbox.
* Map messages as simple `.txt` files.

### Milestone 2: The Markdown & Search Layer
* Implement the `body.md` conversion logic.
* Implement the `/search` magic directory.
* Add `moka` caching for metadata.

### Milestone 3: Advanced Operations
* Support for attachments.
* Support for trashing/moving emails via `mv` and `rm`.
* Pub/Sub integration for real-time updates.

---

## 6. Prompt for gemini-cli
> *"I want to build a Rust program using the `fuser` crate that maps the Gmail API to a filesystem. Use the provided PRD to scaffold the project structure. Start by defining the `Inode` mapping strategy and the `lookup` and `readdir` functions for the root and `/inbox` directories. Ensure it uses `tokio` for async API calls."*



## Implementation Sketch Notes

To build a robust Gmail-as-a-FileSystem (GmailFS) in Rust, we need to solve the mismatch between a "stateless" search API and the "stateful" nature of a directory tree, while ensuring the agent doesn't drown in HTML tag soup.

---

## 1. Implementing the "Magic" Search Directory
A search in Gmail is a dynamic collection, not a static folder. To let an agent use this, you can implement **Virtual Query Nodes**.

### How it works:
1.  **The Trigger:** The agent creates a directory or writes to a special file:
    * `mkdir /search/"from:boss project-alpha"`
    * *OR* `echo "from:boss project-alpha" > /search/.query`
2.  **The Execution:** Your Rust program intercepts this `mkdir` or `write` call. It uses the string as a `q` parameter for the Gmail `messages.list` API.
3.  **The Population:** The filesystem then populates that directory with symlinks to the actual messages found in `/all_mail/`.

### Caching Strategy
Since agents might `ls` the same search directory repeatedly, you need a multi-tiered cache:
* **Metadata Cache (`moka` or `dashmap`):** Store the mapping of `Query String -> [Message IDs]` with a Short TTL (e.g., 60 seconds).
* **Inode Persistence:** Assign fixed Inodes to messages so that if an agent runs `stat` on an email in a search result, it matches the same file in the Inbox.
* **Negative Caching:** If a search returns zero results, cache that "empty" state for a few seconds to prevent the agent from spamming the API in a loop if it's waiting for a specific email.

---

## 2. Presenting Content: The "Clean View" Pattern
Raw email data is a mess of MIME parts and nested HTML. An agent reading a `cat /inbox/email.txt` will waste tokens and lose context if it sees `<div>` tags and CSS.

### Proposed File Layout
Instead of one file per email, represent an email as a **directory** to give the agent choices:
```text
/inbox/
└── 2024-04-02_Subject_Line/
    ├── body.md        <-- Primary choice (Markdown)
    ├── body.html      <-- Fallback (Raw HTML)
    ├── metadata.json  <-- Headers, Date, From, To
    └── attachments/
        └── invoice.pdf
```

### The Markdown Conversion Pipeline
In Rust, you can use the `html-to-markdown-rs` crate or `pandoc` bindings to transform the body on the fly:
1.  **Extract:** Pull the `text/html` part from the Gmail MIME response.
2.  **Sanitize:** Strip script tags, styles, and tracking pixels.
3.  **Convert:** Turn tables and links into Markdown. This preserves the *structure* (which agents need for reasoning) without the *bloat*.
4.  **Lazy Loading:** Don't convert the HTML until the agent actually calls `read()` on `body.md`.

---

## 3. Rust-Specific Implementation Tips

### Handling "Write" Operations
If an agent wants to "reply," it shouldn't have to construct a complex MIME message. You can simplify the FS interface:
* **Drafting:** The agent creates a file in `/drafts/new_reply.md`.
* **Sending:** Moving a file into a special `.send` folder could trigger the `messages.send` API call.

### The "Reactive" Cache
Use a background task (via `tokio::spawn`) that listens for Gmail **Push Notifications (Pub/Sub)**. When a new mail arrives, the Rust program invalidates the local cache for `/inbox`. This ensures that when the agent runs `ls`, the data is already "warm" and it doesn't have to wait for a round-trip to Google's servers.

---

### Comparison of Presentation Formats for Agents

| Format | Token Cost | Structural Clarity | Best Use Case |
| :--- | :--- | :--- | :--- |
| **Raw Text** | Lowest | Poor (lost links/tables) | Simple notifications |
| **HTML** | Very High | High (but noisy) | Never (unless searching for hidden metadata) |
| **Markdown** | **Medium** | **High** | **Most agentic workflows (Default)** |
| **JSON** | Medium | Excellent | Extracting specific headers/dates |

833638d7-438f-4a32-833d-f543a3f58d2f