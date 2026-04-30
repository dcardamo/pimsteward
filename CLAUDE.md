# pimsteward

## Deploying changes to production

After modifying pimsteward and deploying to the pimsteward container, you MUST verify end-to-end from inside the rockycc container before telling Dan it works. "The server is running" is not verification. "The service responds to curl" is not verification. You must verify that Rocky's Claude Code session actually has the tools registered with the correct schemas.

**This applies per-daemon.** pimsteward runs as a separate daemon per provider — one for forwardemail, another for iCloud CalDAV — each with its own container, port, bearer token file, git repo, and entry in Rocky's `.mcp.json`. After a deploy, verify the daemon you actually changed; deploying a forwardemail-side fix does not exercise the iCloud daemon and vice versa. The verification steps below are written against the forwardemail daemon as the canonical example; the iCloud-specific block follows.

### Verification steps (all required)

1. **Run the verification script** from the host:
   ```
   sudo machinectl shell --uid=1000 rockycc /bin/sh -c "bash /rockycc/scripts/verify-pimsteward.sh"
   ```
   This checks server-side (config, network, auth, MCP protocol) AND client-side (Rocky's Claude session shows tools as connected). All checks must pass.

2. **Verify tool schemas** if you changed tool parameters. Do a full MCP handshake + tools/list from inside rockycc and confirm the new parameters appear in the schema:
   ```
   sudo machinectl shell --uid=1000 rockycc /bin/sh -c '
   URL="http://10.0.102.2:8101/mcp"
   AUTH=$(jq -r ".mcpServers[\"pimsteward-dan\"].headers.Authorization" /rockycc/.mcp.json)
   RESP=$(curl -si --max-time 5 -X POST "$URL" \
     -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
     -H "Authorization: $AUTH" \
     -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"schema-check\",\"version\":\"1.0\"}}}" 2>/dev/null)
   SESSION=$(echo "$RESP" | grep -i "mcp-session-id" | awk -F": " "{print \$2}" | tr -d "\r\n")
   curl -s -X POST "$URL" -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
     -H "Authorization: $AUTH" -H "Mcp-Session-Id: $SESSION" \
     -d "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}" > /dev/null
   sleep 1
   curl -s -X POST "$URL" -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
     -H "Authorization: $AUTH" -H "Mcp-Session-Id: $SESSION" \
     -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}" | \
     grep -o "data: {.*}" | sed "s/^data: //" | jq ".result.tools[] | select(.name == \"THE_TOOL_YOU_CHANGED\")"
   '
   ```

3. **Restart Rocky's session** after any pimsteward deploy that changes tool schemas. MCP tools are cached at session start — a pimsteward restart drops the HTTP connection but Rocky's Claude session keeps the stale schema. Kill tmux and restart:
   ```
   sudo machinectl shell rockycc /bin/sh -c "tmux -L rockycc kill-server; sleep 2; systemctl restart rockycc-main"
   ```
   Then wait 15 seconds and re-run the verification script.

### Known gotchas

- **`type: "http"` required in .mcp.json** — Claude Code's MCP config schema requires `"type": "http"` for HTTP/SSE servers. Without it, entries silently fail to parse and tools never register even though the server is running fine.
- **Bearer token must exist** — The token file must be deployed via dotvault to BOTH the pimsteward container (server) and rockycc container (client). Check both: `ls /var/lib/pimsteward-secrets/.config/secrets/pimsteward-mcp-bearer-token` and the rockycc side.
- **Rocky's session caches tool schemas** — After a pimsteward binary update, Rocky must restart to pick up new/changed tool parameters. The verification script checks "connected" but you must also verify schemas if you changed params.

### Verifying the iCloud daemon

The iCloud CalDAV daemon is a *separate* pimsteward instance from the forwardemail one — separate container, separate systemd unit, separate port, separate bearer token, separate git repo, and a separate entry in Rocky's `.mcp.json` (e.g. `pimsteward-icloud-dan` rather than `pimsteward-dan`). Verifying after an iCloud-side change is the same shape as the forwardemail flow above, just pointed at the iCloud daemon's address.

**The example values below are placeholders.** The actual port, bearer-token file path, container name, and `.mcp.json` key need to match the operator's systemd unit file and `.mcp.json` — substitute whatever is actually deployed. If unsure, read the iCloud daemon's `systemctl cat <unit>` output and Rocky's `.mcp.json` before running anything.

1. **Run the verification script** against the iCloud daemon (placeholder port `8102`):
   ```
   sudo machinectl shell --uid=1000 rockycc /bin/sh -c "PIMSTEWARD_PORT=8102 PIMSTEWARD_MCP_KEY=pimsteward-icloud-dan bash /rockycc/scripts/verify-pimsteward.sh"
   ```
   If the verify script doesn't yet take those env vars, run the same MCP handshake manually (see step 2 below) against the iCloud port and bearer token instead.

2. **Verify tool schemas + provider-correctness.** Same handshake as the forwardemail block, just pointed at the iCloud daemon. After `tools/list`, additionally call `list_calendars` and confirm the response is the iCloud calendars (e.g. `pimsteward_test`, your iCloud "Home", "Work" calendars), **not** the forwardemail calendar set. A swapped bearer-token or proxied port can make the script appear healthy while talking to the wrong daemon — the calendar list is the unambiguous tell.
   ```
   sudo machinectl shell --uid=1000 rockycc /bin/sh -c '
   URL="http://10.0.102.2:8102/mcp"     # placeholder — use the iCloud daemon port
   AUTH=$(jq -r ".mcpServers[\"pimsteward-icloud-dan\"].headers.Authorization" /rockycc/.mcp.json)
   # ... same initialize / notifications/initialized / tools/list dance as above ...
   # then call list_calendars and verify the calendars are iCloud, not forwardemail
   '
   ```

3. **Restart Rocky's session** if the iCloud-side change touched tool schemas — same caching gotcha as forwardemail. Rocky's MCP client caches each server's tool schema at session start; a redeploy of the iCloud daemon drops its HTTP connection but the cached schema sticks until tmux is killed:
   ```
   sudo machinectl shell rockycc /bin/sh -c "tmux -L rockycc kill-server; sleep 2; systemctl restart rockycc-main"
   ```
   Then wait 15 seconds and re-run the iCloud verification above.

The "Known gotchas" above (`type: "http"` requirement, bearer-token file deployment, schema caching) all apply identically to the iCloud daemon — they're properties of the MCP client / Claude Code config, not the provider.

## Architecture

Single daemon process serves both pull loops and MCP HTTP. No stdio transport. See `src/daemon.rs`. One daemon per provider — the forwardemail and iCloud daemons run side by side, each with its own port, repo, credentials, and MCP endpoint.

- `pimsteward daemon --port 8100` = pulls + MCP HTTP (one provider)
- `pimsteward daemon` (no --port) = pulls only
- AI clients connect via HTTP SSE with bearer token auth
- `get_permissions` tool lets clients introspect access levels (per-daemon — calendar-only on the iCloud daemon, full surface on forwardemail)
