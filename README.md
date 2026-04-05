# pimsteward

**pimsteward is a PIM steward for [forwardemail.net](https://forwardemail.net)** — a
permission-aware MCP mediator between an AI assistant and your mail, calendar,
contacts, and sieve rules, with time-travel backup built in.

## Status

Early development. See [PLAN.md](PLAN.md) for the full design and
implementation phases.

## What it does

- **Mediates.** Your AI assistant talks to pimsteward over MCP, not to
  forwardemail directly. pimsteward holds the credentials, enforces a
  per-resource permission policy, and attributes every write so you can see
  exactly what the AI changed.
- **Backs up.** Every change to your calendars, contacts, mail, or sieve
  scripts lands in a local git repository as a time-series log — whether the
  change came from your AI, your IMAP/CalDAV client, or forwardemail itself.
- **Restores.** Rewind any file, directory, or date range back to a prior
  state, selectively. Your AI can drive the restore too, through a dry-run
  tool that requires explicit confirmation to apply.

## Why

Giving an AI assistant access to your personal data is a one-way trust
decision unless you have receipts. pimsteward is the receipts layer. If the
AI deletes all your events next month, you restore them. If you just want to
see what it changed today, you ask git.

## Non-goals

- Not a generic backup tool — use restic/borg for disk-level backup.
- Not a PIM client — keep using your favourite IMAP/CalDAV app.
- Not a multi-provider sync tool — v1 is forwardemail-only.

## License

MIT. See [LICENSE](LICENSE).
