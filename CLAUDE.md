# pimsteward

## Deploying changes to production

After modifying pimsteward and deploying to the pimsteward container, you MUST verify end-to-end from inside the rockycc container before telling Dan it works. "The server is running" is not verification. "The service responds to curl" is not verification. You must verify that Rocky's Claude Code session actually has the tools registered with the correct schemas.

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

## Architecture

Single daemon process serves both pull loops and MCP HTTP. No stdio transport. See `src/daemon.rs`.

- `pimsteward daemon --port 8100` = pulls + MCP HTTP
- `pimsteward daemon` (no --port) = pulls only
- AI clients connect via HTTP SSE with bearer token auth
- `get_permissions` tool lets clients introspect access levels
