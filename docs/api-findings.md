# forwardemail.net API findings

_Smoke-tested 2026-04-05 against `dotfiles_mcp_test@purpose.dev` on the live
`https://api.forwardemail.net` instance. These findings drive the pimsteward
implementation — revisit if forwardemail changes behaviour._

## TL;DR

| Area                       | Verdict                                                     | Impact on pimsteward                                   |
| -------------------------- | ----------------------------------------------------------- | ------------------------------------------------------ |
| Alias auth                 | ✅ works (HTTP Basic, alias email + generated password)     | standard reqwest basic auth, nothing special           |
| Rate limiting              | ✅ 1000/window, `X-RateLimit-*` headers present             | watch `Remaining`, back off at <100                    |
| ETag header                | ✅ on every response                                        | we can use it for caching + If-Match writes            |
| If-Match optimistic writes | ✅ works on contacts: stale → **HTTP 412**                  | restore tool safety guarantee holds                    |
| Raw content exposure       | ✅ contacts expose `content` field with raw vCard           | store bytes verbatim, no serialisation round-trip      |
| Message body mutability    | ❌ `PUT /v1/messages/:id` with `{raw: ...}` silently ignored| mail store: `.eml` write-once, only `meta.json` mutates|
| Flag/folder mutations      | ✅ `PUT /v1/messages/:id` with `{flags: [...]}` works       | expected IMAP-like semantics                           |
| Sieve CRUD                 | ✅ field is `content`, server validates syntax              | forwardemail returns `is_valid`, `required_capabilities`, `security_warnings` |
| Search by `since`          | ✅ filters on `header_date >= since`                        | good enough for incremental mail pull                  |
| Search by `subject`, `from`, `to`, etc. | ✅ substring matching                           | pass through from MCP `search_email`                   |
| Modseq / UIDVALIDITY       | ✅ exposed on message + folder JSON                         | can do CONDSTORE-style delta sync via REST             |
| Calendar event creation    | ⚠ unresolved — correct payload shape TBD                    | blocks calendar write tools until figured out          |

## Details

### Auth

```
curl -u '<alias_email>:<generated_password>' https://api.forwardemail.net/v1/account
```

Returns `{"object":"alias", "email":"…", "storage_used":…, "storage_quota":…,
"has_imap":true, …}`. The `GET /v1/account` endpoint is the cheapest
keepalive probe for the client's connection pool.

### Rate limiting

Every response includes:

```
X-RateLimit-Limit: 1000
X-RateLimit-Remaining: 976
X-RateLimit-Reset: 1775359712   # unix timestamp, seconds
```

Observed across ~40 requests in a few minutes, `Remaining` decremented
predictably. `Reset` advanced by ~1/sec, suggesting a sliding window. No 429s
hit during testing. Conservative client policy: back off exponentially when
`Remaining < 100`, and don't retry on 429 without honoring `Retry-After`.

### ETag + If-Match (optimistic concurrency)

**Contacts:**

```
POST /v1/contacts { full_name: "X", emails: [...] }
→ 200, body includes: etag: "1f6b9549224f62b9f0d4f613c57b16f6"
→ headers also include: ETag: "1f6b9549224f62b9f0d4f613c57b16f6"
```

Updates honor `If-Match` correctly:

| PUT variant                          | Result                            |
| ------------------------------------ | --------------------------------- |
| `PUT /v1/contacts/:id` (no header)   | 200 OK, last-writer-wins          |
| `PUT` with `If-Match: "<current>"`   | 200 OK, update applied            |
| `PUT` with `If-Match: "<stale>"`     | **412 Precondition Failed**       |

**Calendar events:** ETag header present on GET. Couldn't test If-Match
round-trip because event creation payload wasn't resolved. Assume the same
semantics until proven otherwise.

**Messages:** Less relevant — the only mutable fields are flags/folder, and
concurrent flag updates are safe to last-writer-win.

**Sieve scripts:** ETag header present on GET. Not yet tested with PUT If-Match.

### Contact resource shape

```json
{
  "id": "69d1d638557459889303f492",
  "uid": "69d1d638557459889303f493",
  "full_name": "Smoke Test",
  "content": "BEGIN:VCARD\nVERSION:3.0\nUID:...\nFN:...\nEMAIL;TYPE=home:...\nEND:VCARD",
  "etag": "\"1f6b9549224f62b9f0d4f613c57b16f6\"",
  "is_group": false,
  "emails": [{"value": "...", "type": "home", "_id": "..."}],
  "phone_numbers": [],
  "created_at": "...",
  "updated_at": "...",
  "object": "contact"
}
```

The `content` field is the raw vCard text. **pimsteward stores this verbatim
as `contacts/<book>/<uid>.vcf`** — no parse/serialize round-trip, no
canonicalisation needed, byte-identical to what a CardDAV client would see.

### Sieve script resource shape

```json
{
  "object": "sieve_script",
  "id": "69d1d69d492ecc5560bd62ee",
  "name": "smoke3",
  "content": "require [\"fileinto\"];\nif header :contains \"subject\" \"smoke\" { fileinto \"Junk\"; }",
  "description": "",
  "is_active": false,
  "is_valid": true,
  "required_capabilities": ["fileinto"],
  "security_warnings": [],
  "validation_errors": [],
  "created_at": "...",
  "updated_at": "..."
}
```

Correct create field is `content` (not `script`). Server parses and validates
on create — `is_valid`, `required_capabilities`, `security_warnings` come
back automatically. This is pimsteward's `install_sieve_script` tool:
dry-run validation for free.

### Message resource shape

```json
{
  "id": "69d1d63bc4828ace17532557",
  "root_id": "...", "thread_id": "...", "folder_id": "...", "folder_path": "INBOX",
  "header_message_id": "<smoke-test-1@example.com>",
  "is_unread": false, "is_flagged": true, "is_deleted": false,
  "is_draft": false, "is_junk": false, "is_encrypted": false,
  "has_attachment": false,
  "retention_date": "...", "internal_date": "...", "header_date": "...",
  "subject": "smoke test",
  "flags": ["\\Seen", "\\Flagged"],
  "labels": [],
  "size": 174,
  "uid": 1,
  "modseq": 1,
  "transaction": "API",
  "remote_address": "174.89.36.119",
  "created_at": "...",
  "updated_at": "...",
  "nodemailer": { ... full parsed MIME structure ... }
}
```

Key fields for delta sync:

- **`modseq`** — IMAP CONDSTORE modseq counter. Increments on flag changes.
  pimsteward can use this for efficient mail delta sync: query `modseq > N`
  style (if the API supports the filter), else just include modseq in each
  fetched message's meta and diff locally.
- **`uid` / `uid_validity`** (uid_validity on the folder) — stable message
  identifier within a folder. Survives flag changes.
- **`internal_date`** vs **`header_date`** vs **`updated_at`** — three
  different timestamps. `internal_date` is when the message arrived,
  `header_date` is the Date: header, `updated_at` changes on flag updates.
  For the pimsteward pull loop, track `updated_at` to detect flag/folder
  changes since last poll.

### Message mutability

**Flags/folder: mutable.** `PUT /v1/messages/:id` with `{flags: [...]}` or
`{folder: "X"}` works as expected. Verified with a round-trip: created, set
`\Seen` and `\Flagged`, re-fetched, flags present.

**Body: immutable.** `PUT /v1/messages/:id` with `{raw: "..."}` returns
200 OK but the subject, size, and body don't change. This confirms the
pimsteward storage plan:

```
mail/<folder_id>/<msg_id>/raw.eml      ← write once from initial fetch
mail/<folder_id>/<msg_id>/meta.json    ← updated on every flag/folder change
```

Never re-fetch `raw.eml` after the first successful fetch. Attachments are
referenced from `nodemailer.attachments` and stored by content hash.

### Folder resource shape

```json
{
  "id": "69d1c06a5632a2f066c4f96f",
  "path": "INBOX",
  "name": "INBOX",
  "parent": null,
  "uid_validity": 1775353962,
  "uid_next": 1,
  "modify_index": 0,
  "subscribed": true,
  "flags": [],
  "retention": 0,
  "special_use": "\\Inbox",
  "created_at": "...",
  "updated_at": "...",
  "object": "folder"
}
```

The `uid_validity` lets pimsteward detect IMAP folder re-creation (classic
IMAP thing — if UIDVALIDITY changes, all UIDs are invalidated and we must
re-sync the folder from scratch). `uid_next` tells us the next UID to
expect. `modify_index` is the folder-level modseq — can be used as a "has
anything in this folder changed since my last poll" check without iterating
messages.

### Search parameters

Confirmed working on `GET /v1/messages`:

- `since=<ISO>` — messages where `header_date >= since`
- `before=<ISO>` — messages where `header_date <= before`
- `subject=<string>` — substring match (case-insensitive per the marketing
  claims, not explicitly tested)
- `folder=<id or path>` — filter to one folder

Per the forwardemail blog post, the full search parameter set is 15+ including
`from`, `to`, `text` (body), `is_unread`, `is_flagged`, `has_attachments`,
`min_size`, `max_size`, `headers`, `message_id`. Pass these through from
`search_email` without re-implementing.

### Unresolved: calendar event creation payload

Tried three shapes; all rejected:

1. `{"calendar": "<id>", "summary": "...", "dtstart": "..."}` → 400
   "Calendar ID is required." — wrong field name.
2. `POST /v1/calendars/:id/events` with structured event fields → 404
   (route doesn't exist).
3. `{"calendar_id": "<id>", "content": "BEGIN:VCALENDAR\nVERSION:2.0\n..."}` → 400
   "iCal data is required." — field name likely `content` but iCal payload
   was probably malformed (missing DTSTAMP, CALSCALE, etc.).

**Resolution plan:** in task #6 phase B, read forwardemail's public source
at https://github.com/forwardemail/forwardemail.net for the exact handler
signature, or generate a minimal valid iCalendar payload with
`icalendar-rs` and re-test. This does not block the pull loop, only the
`create_event` write tool.

### Error shape

```json
{
  "statusCode": 400,
  "error": "Bad Request",
  "message": "Calendar ID is required."
}
```

Consistent across endpoints. pimsteward's `forwardemail::Error` enum can
match on `statusCode` for structured handling. 412 returns the same shape
with `"message": "Precondition Failed"`.

### Pagination

`Link` header rel="next"/"prev"/"first"/"last" (GitHub-style), plus
`X-Page-Count`, `X-Page-Current`, `X-Page-Size`, `X-Item-Count`. Default
page size 10, max 50 (per the reference docs — not explicitly tested here).
For the pull loop, we iterate pages until `rel="next"` is absent or
`X-Page-Current == X-Page-Count`.

### Observed response times

`X-Response-Time` header is present on every response. Observed values:
`account` ~5-15ms, list endpoints ~40-70ms, create/update ~40-100ms. Fast
enough that naive synchronous polling is fine for cadences measured in
minutes.

### Headers summary (copy for the reqwest client)

Request headers to send:

```
Authorization: Basic <base64(alias:password)>
Content-Type: application/json        # for POST/PUT
Accept-Language: en                    # optional, defaults to en
If-Match: "<etag>"                     # on PUT/DELETE where concurrency matters
User-Agent: pimsteward/<version>       # good manners
```

Response headers to parse:

```
ETag                      # store for optimistic writes
X-RateLimit-Limit
X-RateLimit-Remaining     # back off when low
X-RateLimit-Reset
X-Page-Count
X-Page-Current
X-Item-Count
Link                      # for pagination
X-Request-Id              # include in tracing spans for log correlation
```

## Open questions that do NOT block pimsteward v1

- Is there a `modseq > N` filter on `GET /v1/messages`? Would make delta
  sync O(changes) instead of O(messages). Test during phase B implementation.
- Does `PUT /v1/sieve-scripts/:id` honor `If-Match`? Assume yes, verify.
- Does `POST /v1/messages` with `{raw: ...}` preserve arbitrary headers, or
  does forwardemail rewrite/strip any? Test with a message that has unusual
  headers (X-Custom-*, List-Unsubscribe, etc.).
- CardDAV ETag format: `"\"hash\""` vs `"hash"` — the JSON field has quoted
  outer quotes and the header does not. Normalise to the header form when
  round-tripping.
- Does forwardemail ever return different ETags for byte-identical resources
  (e.g. because of internal metadata)? Observed stable during this test;
  would need longer observation to be sure.

## Test cleanup

All resources created during the smoke test were deleted in the same script
runs. Verified via `GET /v1/{calendars,contacts,messages,sieve-scripts}`
returning `[]` after cleanup. No lingering state on `dotfiles_mcp_test@purpose.dev`.
