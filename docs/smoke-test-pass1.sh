#!/usr/bin/env bash
# forwardemail.net smoke test against dotfiles_mcp_test@purpose.dev
# Answers the five open questions from pimsteward/PLAN.md before implementation.
set -uo pipefail

USER=$(cat ~/.config/secrets/pimsteward-test-alias-user)
PASS=$(cat ~/.config/secrets/pimsteward-test-alias-password)
BASE="https://api.forwardemail.net"
AUTH="-u $USER:$PASS"
OUT=/tmp/fe-smoke-findings.md

say() { echo -e "\n\n## $*\n" | tee -a "$OUT"; }
run() {
  local label="$1"; shift
  echo -e "\n### $label\n\n\`\`\`" | tee -a "$OUT"
  echo "$ $*" | tee -a "$OUT"
  "$@" 2>&1 | tee -a "$OUT"
  echo '```' | tee -a "$OUT"
}
note() { echo -e "\n$*\n" | tee -a "$OUT"; }

: > "$OUT"
echo "# forwardemail smoke test findings" | tee -a "$OUT"
echo "_Alias: $USER, date: $(date -Iseconds)_" | tee -a "$OUT"

#############################################
say "Q0: Basic auth + account endpoint"
#############################################
run "GET /v1/account" curl -sS $AUTH -w "\nHTTP %{http_code}\n" "$BASE/v1/account"

#############################################
say "Q1: List calendars"
#############################################
CAL_RESP=$(curl -sS $AUTH -D /tmp/cal-headers.txt "$BASE/v1/calendars")
echo "$CAL_RESP" | jq -C . 2>&1 | head -60 | tee -a "$OUT"
echo "response headers:" | tee -a "$OUT"
cat /tmp/cal-headers.txt | tee -a "$OUT"

# Get first calendar ID
CAL_ID=$(echo "$CAL_RESP" | jq -r 'if type=="array" and length>0 then .[0].id // .[0]._id else "" end')
note "First calendar id: $CAL_ID"

#############################################
say "Q2: List calendar events (for rate-limit + churn + ETag observation)"
#############################################
run "GET /v1/calendar-events (with full headers)" curl -sS $AUTH -D /tmp/ev-headers.txt "$BASE/v1/calendar-events?limit=5"
echo '### response headers:' | tee -a "$OUT"
cat /tmp/ev-headers.txt | tee -a "$OUT"
note "Looking for: X-RateLimit-*, ETag, Last-Modified headers"

#############################################
say "Q3: Create a test calendar event, then read it twice to check iCal churn"
#############################################
if [ -n "$CAL_ID" ]; then
    # Create a minimal event
    EVENT_PAYLOAD='{
      "calendar": "'"$CAL_ID"'",
      "summary": "pimsteward smoke test event",
      "dtstart": "2027-01-15T10:00:00Z",
      "dtend": "2027-01-15T11:00:00Z"
    }'
    CREATE_RESP=$(curl -sS $AUTH -D /tmp/create-headers.txt \
        -X POST "$BASE/v1/calendar-events" \
        -H "Content-Type: application/json" \
        -d "$EVENT_PAYLOAD")
    echo "create response:" | tee -a "$OUT"
    echo "$CREATE_RESP" | jq -C . 2>&1 | head -40 | tee -a "$OUT"
    echo "create headers:" | tee -a "$OUT"
    cat /tmp/create-headers.txt | tee -a "$OUT"

    EVENT_ID=$(echo "$CREATE_RESP" | jq -r '.id // ._id // empty')
    note "Event ID: $EVENT_ID"

    if [ -n "$EVENT_ID" ]; then
        say "Q3a: Fetch event twice back-to-back — check for iCal churn"
        curl -sS $AUTH "$BASE/v1/calendar-events/$EVENT_ID" > /tmp/event-fetch-1.json
        sleep 1
        curl -sS $AUTH "$BASE/v1/calendar-events/$EVENT_ID" > /tmp/event-fetch-2.json
        if diff -q /tmp/event-fetch-1.json /tmp/event-fetch-2.json > /dev/null; then
            note "**FINDING Q3a:** sequential GETs are byte-identical — no server-side churn."
        else
            note "**FINDING Q3a:** sequential GETs differ! diff:"
            diff /tmp/event-fetch-1.json /tmp/event-fetch-2.json | head -30 | tee -a "$OUT"
        fi
        echo "fetch 1 body (head):" | tee -a "$OUT"
        jq -C . /tmp/event-fetch-1.json 2>&1 | head -40 | tee -a "$OUT"

        say "Q3b: ETag round-trip — does the API send one, and does If-Match work?"
        curl -sS $AUTH -D /tmp/h1.txt -o /tmp/ev1.json "$BASE/v1/calendar-events/$EVENT_ID"
        ETAG=$(grep -i '^etag:' /tmp/h1.txt | awk '{print $2}' | tr -d '\r')
        note "ETag from GET: '$ETAG'"

        # Try update with matching If-Match
        if [ -n "$ETAG" ]; then
            UPDATE_PAYLOAD='{"summary":"pimsteward smoke test event (updated)"}'
            run "PUT with If-Match: $ETAG" curl -sS $AUTH \
                -X PUT "$BASE/v1/calendar-events/$EVENT_ID" \
                -H "Content-Type: application/json" \
                -H "If-Match: $ETAG" \
                -d "$UPDATE_PAYLOAD" \
                -w "\nHTTP %{http_code}\n"

            # Try update with stale If-Match (expect 412)
            run 'PUT with If-Match: "stale"' curl -sS $AUTH \
                -X PUT "$BASE/v1/calendar-events/$EVENT_ID" \
                -H "Content-Type: application/json" \
                -H 'If-Match: "definitely-not-the-current-etag"' \
                -d "$UPDATE_PAYLOAD" \
                -w "\nHTTP %{http_code}\n"
        else
            note "**FINDING Q3b:** no ETag header returned — optimistic concurrency unavailable via standard HTTP. Must rely on last-writer-wins."
        fi

        say "Q3c: Delete the test event to clean up"
        run "DELETE /v1/calendar-events/$EVENT_ID" curl -sS $AUTH -X DELETE "$BASE/v1/calendar-events/$EVENT_ID" -w "\nHTTP %{http_code}\n"
    else
        note "**FINDING:** create did not return an event id; write path unclear"
    fi
else
    note "**FINDING:** no calendars exist on this alias; cannot test calendar-events round-trip"
fi

#############################################
say "Q4: List folders + messages"
#############################################
run "GET /v1/folders" curl -sS $AUTH -D /tmp/fld-headers.txt "$BASE/v1/folders" -w "\nHTTP %{http_code}\n"
run "GET /v1/messages?limit=3" curl -sS $AUTH -D /tmp/msg-headers.txt "$BASE/v1/messages?limit=3" -w "\nHTTP %{http_code}\n"
echo "message list headers:" | tee -a "$OUT"
cat /tmp/msg-headers.txt | tee -a "$OUT"

#############################################
say "Q5: List contacts"
#############################################
run "GET /v1/contacts" curl -sS $AUTH "$BASE/v1/contacts?limit=3" -w "\nHTTP %{http_code}\n"

#############################################
say "Q6: List sieve scripts"
#############################################
run "GET /v1/sieve-scripts" curl -sS $AUTH "$BASE/v1/sieve-scripts" -w "\nHTTP %{http_code}\n"

#############################################
say "Q7: Rate-limit headers — make 10 rapid requests and look for backoff"
#############################################
for i in $(seq 1 10); do
    curl -sS $AUTH -o /dev/null -D /tmp/rl-$i.txt "$BASE/v1/account"
done
echo "rate limit headers observed across 10 rapid calls:" | tee -a "$OUT"
grep -hi '^x-ratelimit\|^ratelimit\|^retry-after' /tmp/rl-*.txt 2>/dev/null | sort -u | tee -a "$OUT" || echo "(none)"

echo ""
echo "=== DONE — findings at $OUT ==="
