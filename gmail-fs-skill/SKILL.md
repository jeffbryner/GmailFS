---
name: gmail-fs
description: Interact with Gmail through a filesystem interface mounted at /tmp/gmail
---

# Gmail Filesystem Interface

This skill enables interaction with Gmail through a filesystem interface mounted at `/tmp/gmail`.

## Directory Structure

The Gmail filesystem has the following top-level directories:

- `/tmp/gmail/inbox` - All inbox messages
- `/tmp/gmail/outbox` - Directory for sending emails (write files here)
- `/tmp/gmail/unread` - Unread messages only
- `/tmp/gmail/search` - Search interface (create query directories)

## Email Directory Format

Each email is represented as a directory with the naming format:
```
{date}_{subject}_{message_id}/
```

Inside each email directory:
- `body.html` - HTML version of the email
- `body.md` - Markdown-converted body (preferred for reading)
- `snippet.txt` - Short preview text (HTML-encoded)
- `metadata.json` - Full email headers (From, To, Subject, Date, Message-ID, etc.)
- `attachments/` - Directory containing any attachments

## Operations

### Reading Emails

Use the `Read` tool to access email content:

```bash
# Read the markdown body (most readable)
Read /tmp/gmail/inbox/{email_directory}/body.md

# Read metadata
Read /tmp/gmail/inbox/{email_directory}/metadata.json

# Read snippet
Read /tmp/gmail/inbox/{email_directory}/snippet.txt
```

### Listing Emails

Use `Bash` to list emails:

```bash
# List all inbox emails
ls /tmp/gmail/inbox

# List recent emails (sorted by modification time)
ls -lt /tmp/gmail/inbox | head -20

# Search for emails by subject keyword
ls /tmp/gmail/inbox | grep -i "keyword"

# Count unread emails
ls /tmp/gmail/unread | wc -l
```

### Sending Emails

To send an email, write a text file to `/tmp/gmail/outbox/` with this format:

```
To: recipient@example.com
Subject: Email subject line

Body of the email starts here after a blank line.

Can include multiple paragraphs and formatting.
```

**Important:**
- First line must be `To: email@address`
- Second line must be `Subject: subject text`
- Third line must be blank
- Body starts on line 4

Example using Bash:
```bash
cat > /tmp/gmail/outbox/my_email.txt << 'EOF'
To: someone@example.com
Subject: Quick update

Hi there,

This is a test email sent via the filesystem interface.

Best regards
EOF
```

The filesystem will process the file and send the email. The sent email will appear in the inbox.

### Deleting Emails

To delete an email, remove its directory:

```bash
rm -rf /tmp/gmail/inbox/{email_directory}
```

This permanently deletes the email from Gmail.

### Searching (Advanced)

Searches use a two-plane architecture separating search management from results:

#### Management Plane (`/tmp/gmail/saved_searches/`)

Create, manage, and remove search definitions:

```bash
# Create a search
mkdir /tmp/gmail/saved_searches/"from:google"

# Create complex searches with multiple operators
mkdir /tmp/gmail/saved_searches/"from:google after:2026-03-01"
mkdir /tmp/gmail/saved_searches/"subject:invoice has:attachment"

# Drop a search (always empty, so rmdir works)
rmdir /tmp/gmail/saved_searches/"from:google"
```

#### Data Plane (`/tmp/gmail/search/`)

View and interact with search results:

```bash
# View results for a search
ls /tmp/gmail/search/"from:google"

# Read an email from search results
Read /tmp/gmail/search/"from:google"/{email_directory}/body.md

# Count results
ls /tmp/gmail/search/"from:google after:2026-03-01" | wc -l

# Trash all emails from a search
rm -rf /tmp/gmail/search/"from:google after:2026-03-01"
```

**Important:** Use proper Gmail search syntax with colons (`:`) not hyphens (`-`):
- ✅ `from:google` - finds emails from Google
- ❌ `from-google` - searches for the words "from" and "google"

**Common Gmail search operators:**
- `from:sender@domain.com` - emails from specific sender
- `to:recipient@domain.com` - emails to specific recipient
- `subject:keyword` - emails with keyword in subject
- `has:attachment` - emails with attachments
- `is:unread` - unread emails only
- `after:2026-03-01` - emails after a date (YYYY-MM-DD format)
- `before:2026-04-01` - emails before a date
- `newer_than:7d` - emails from last 7 days
- `older_than:1m` - emails older than 1 month
- `filename:pdf` - emails with PDF attachments

**Example workflow:**

```bash
# 1. Create a search for recent important emails
mkdir /tmp/gmail/saved_searches/"from:anthropic after:2026-03-01"

# 2. View the results
ls /tmp/gmail/search/"from:anthropic after:2026-03-01"

# 3. Read a specific email
Read /tmp/gmail/search/"from:anthropic after:2026-03-01"/2026-03-15_Update_*/body.md

# 4. Later, drop the search (keeps emails in inbox)
rmdir /tmp/gmail/saved_searches/"from:anthropic after:2026-03-01"
```

**Key Benefits:**
- **Management plane** (`saved_searches/`) is always empty - `rmdir` works cleanly
- No "File exists" errors when recreating searches
- **Data plane** (`search/`) contains actual email results
- Dropping a search doesn't delete emails
- Can trash emails found by search with `rm -rf` in data plane

## Best Practices

1. **Always read `body.md` for email content** - it's the cleanest format
2. **Check `metadata.json` for headers** - contains sender, recipients, dates
3. **Use `snippet.txt` for quick previews** - useful when scanning many emails
4. **Be careful with deletions** - `rm -rf` on email directories is permanent
5. **Use descriptive filenames in outbox** - helps track what was sent
6. **Check inbox after sending** - sent emails appear there for confirmation

## Common Workflows

### Summarize important unread emails
```bash
# List unread emails
ls /tmp/gmail/unread

# Read specific unread email
Read /tmp/gmail/unread/{email_directory}/body.md
Read /tmp/gmail/unread/{email_directory}/metadata.json
```

### Send a reply
```bash
# Create reply in outbox
cat > /tmp/gmail/outbox/reply.txt << 'EOF'
To: sender@example.com
Subject: Re: Original Subject

Thank you for your email...
EOF

# Verify it was sent by checking inbox
ls -lt /tmp/gmail/inbox | head -5
```

### Clean up old emails
```bash
# List emails by date
ls -lt /tmp/gmail/inbox

# Delete specific email
rm -rf /tmp/gmail/inbox/{email_directory}
```

### Find emails from specific sender
```bash
# Create a search
mkdir /tmp/gmail/saved_searches/"from:sender@example.com"

# View results
ls /tmp/gmail/search/"from:sender@example.com"

# Read specific email from results
Read /tmp/gmail/search/"from:sender@example.com"/{email_directory}/body.md

# Drop search when done
rmdir /tmp/gmail/saved_searches/"from:sender@example.com"
```

## Notes

- The filesystem is mounted at `/tmp/gmail` and requires the Gmail filesystem to be running
- File `00_MOUNT_CHECK_OK` in the root indicates the filesystem is properly mounted
- Email directories use message IDs from Gmail (hex format like `19d67d232f3b3cde`)
- Sent emails appear in your inbox shortly after being written to outbox
- All times are in the system's local timezone
