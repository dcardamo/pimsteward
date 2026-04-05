#!/usr/bin/env bash
# Pass 2: populate resources and test round-trip behaviour.
set -uo pipefail

USER=$(cat ~/.config/secrets/pimsteward-test-alias-user)
PASS=$(cat ~/.config/secrets/pimsteward-test-alias-password)
BASE="https://api.forwardemail.net"
OUT=/tmp/fe-smoke-findings.md

# Append, don't clobber
say() { echo -e "\n\n## PASS 2 — $*\n" | tee -a "$OUT"; }
body_of() { jq -C . "$1" 2>/dev/null | head -50; }

# Helper: call curl, save response body to $1, headers to $2, print http status
call() {
    local body_file="$1"; shift
    local headers_file="$1"; shift
    curl -sS -u "$USER:$PASS" -o "$body_file" -D "$headers_file" -w "HTTP %{http_code}\n" "$@"
}

say "Q1: Create a calendar, then create an event in it"
call /tmp/cal-create.json /tmp/cal-create-h.txt \
    -X POST "$BASE/v1/calendars" \
    -H "Content-Type: application/json" \
    -d '{"name":"pimsteward-smoke-test","color":"#4c9ff5"}'
echo "--- body ---" | tee -a "$OUT"
body_of /tmp/cal-create.json | tee -a "$OUT"
echo "--- headers ---" | tee -a "$OUT"
head -20 /tmp/cal-create-h.txt | tee -a "$OUT"
CAL_ID=$(jq -r '.id // ._id // empty' /tmp/cal-create.json)
echo "CAL_ID=$CAL_ID" | tee -a "$OUT"

if [ -z "$CAL_ID" ]; then
    echo "**FAIL:** could not create calendar. trying list to see if one exists now:" | tee -a "$OUT"
    call /tmp/cal-list.json /tmp/cal-list-h.txt "$BASE/v1/calendars"
    body_of /tmp/cal-list.json | tee -a "$OUT"
    CAL_ID=$(jq -r 'if type=="array" and length>0 then .[0].id else empty end' /tmp/cal-list.json)
    echo "CAL_ID after list=$CAL_ID" | tee -a "$OUT"
fi

if [ -n "$CAL_ID" ]; then
    say "Q2: Create calendar event"
    EVENT_BODY='{
      "calendar": "'"$CAL_ID"'",
      "summary": "pimsteward smoke test event",
      "description": "created by smoke test script",
      "dtstart": "2027-01-15T10:00:00.000Z",
      "dtend": "2027-01-15T11:00:00.000Z",
      "location": "test"
    }'
    call /tmp/ev-create.json /tmp/ev-create-h.txt \
        -X POST "$BASE/v1/calendar-events" \
        -H "Content-Type: application/json" \
        -d "$EVENT_BODY"
    body_of /tmp/ev-create.json | tee -a "$OUT"
    echo "--- headers ---" | tee -a "$OUT"
    head -20 /tmp/ev-create-h.txt | tee -a "$OUT"
    EVENT_ID=$(jq -r '.id // ._id // empty' /tmp/ev-create.json)
    echo "EVENT_ID=$EVENT_ID" | tee -a "$OUT"

    if [ -n "$EVENT_ID" ]; then
        say "Q3: Fetch the same event twice — check for byte-level iCal churn"
        call /tmp/ev-g1.json /tmp/ev-g1-h.txt "$BASE/v1/calendar-events/$EVENT_ID"
        sleep 2
        call /tmp/ev-g2.json /tmp/ev-g2-h.txt "$BASE/v1/calendar-events/$EVENT_ID"
        echo "--- fetch 1 body ---" | tee -a "$OUT"
        body_of /tmp/ev-g1.json | tee -a "$OUT"
        echo "--- fetch 1 headers (ETag, Last-Modified) ---" | tee -a "$OUT"
        grep -iE '^(etag|last-modified):' /tmp/ev-g1-h.txt | tee -a "$OUT"
        echo "--- byte diff between fetches ---" | tee -a "$OUT"
        if diff -q /tmp/ev-g1.json /tmp/ev-g2.json > /dev/null; then
            echo "**FINDING Q3:** byte-identical. No server-side churn on sequential GETs." | tee -a "$OUT"
        else
            echo "**FINDING Q3:** DIFFERS on second fetch:" | tee -a "$OUT"
            diff /tmp/ev-g1.json /tmp/ev-g2.json | head -40 | tee -a "$OUT"
        fi

        ETAG=$(grep -i '^etag:' /tmp/ev-g1-h.txt | awk '{print $2}' | tr -d '\r')
        echo "ETag observed: $ETAG" | tee -a "$OUT"

        say "Q4: PUT with correct If-Match"
        call /tmp/ev-u1.json /tmp/ev-u1-h.txt \
            -X PUT "$BASE/v1/calendar-events/$EVENT_ID" \
            -H "Content-Type: application/json" \
            -H "If-Match: $ETAG" \
            -d '{"summary":"pimsteward smoke test event (updated with if-match)"}'
        echo "--- response ---" | tee -a "$OUT"
        head -5 /tmp/ev-u1-h.txt | tee -a "$OUT"
        body_of /tmp/ev-u1.json | tee -a "$OUT"

        say "Q5: PUT with stale If-Match (expect 412 Precondition Failed)"
        call /tmp/ev-u2.json /tmp/ev-u2-h.txt \
            -X PUT "$BASE/v1/calendar-events/$EVENT_ID" \
            -H "Content-Type: application/json" \
            -H 'If-Match: "completely-bogus-etag"' \
            -d '{"summary":"should fail"}'
        echo "--- response ---" | tee -a "$OUT"
        head -5 /tmp/ev-u2-h.txt | tee -a "$OUT"
        body_of /tmp/ev-u2.json | tee -a "$OUT"

        say "Q6: DELETE the event to clean up"
        call /tmp/ev-d.json /tmp/ev-d-h.txt -X DELETE "$BASE/v1/calendar-events/$EVENT_ID"
        head -3 /tmp/ev-d-h.txt | tee -a "$OUT"
    fi

    say "Q7: DELETE the test calendar to clean up"
    call /tmp/cal-d.json /tmp/cal-d-h.txt -X DELETE "$BASE/v1/calendars/$CAL_ID"
    head -3 /tmp/cal-d-h.txt | tee -a "$OUT"
fi

say "Q8: Create a contact, round-trip, update with ETag, delete"
call /tmp/ct-create.json /tmp/ct-create-h.txt \
    -X POST "$BASE/v1/contacts" \
    -H "Content-Type: application/json" \
    -d '{"full_name":"Smoke Test","emails":[{"type":"home","value":"smoke@example.com"}]}'
body_of /tmp/ct-create.json | tee -a "$OUT"
head -10 /tmp/ct-create-h.txt | tee -a "$OUT"
CONTACT_ID=$(jq -r '.id // ._id // empty' /tmp/ct-create.json)
echo "CONTACT_ID=$CONTACT_ID" | tee -a "$OUT"

if [ -n "$CONTACT_ID" ]; then
    call /tmp/ct-g1.json /tmp/ct-g1-h.txt "$BASE/v1/contacts/$CONTACT_ID"
    echo "--- contact fetch ---" | tee -a "$OUT"
    body_of /tmp/ct-g1.json | tee -a "$OUT"
    grep -iE '^(etag|last-modified):' /tmp/ct-g1-h.txt | tee -a "$OUT"

    # Fetch twice to check churn
    sleep 1
    call /tmp/ct-g2.json /tmp/ct-g2-h.txt "$BASE/v1/contacts/$CONTACT_ID"
    if diff -q /tmp/ct-g1.json /tmp/ct-g2.json > /dev/null; then
        echo "**FINDING:** contact bytes stable across GETs" | tee -a "$OUT"
    else
        echo "**FINDING:** contact bytes differ:" | tee -a "$OUT"
        diff /tmp/ct-g1.json /tmp/ct-g2.json | head -20 | tee -a "$OUT"
    fi

    ETAG=$(grep -i '^etag:' /tmp/ct-g1-h.txt | awk '{print $2}' | tr -d '\r')
    echo "contact ETag=$ETAG" | tee -a "$OUT"

    call /tmp/ct-u.json /tmp/ct-u-h.txt \
        -X PUT "$BASE/v1/contacts/$CONTACT_ID" \
        -H "Content-Type: application/json" \
        -H "If-Match: $ETAG" \
        -d '{"full_name":"Smoke Test Updated"}'
    echo "--- update response ---" | tee -a "$OUT"
    head -3 /tmp/ct-u-h.txt | tee -a "$OUT"
    body_of /tmp/ct-u.json | tee -a "$OUT"

    call /tmp/ct-d.json /tmp/ct-d-h.txt -X DELETE "$BASE/v1/contacts/$CONTACT_ID"
    head -3 /tmp/ct-d-h.txt | tee -a "$OUT"
fi

say "Q9: Create a sieve script, activate, delete"
call /tmp/sv-create.json /tmp/sv-create-h.txt \
    -X POST "$BASE/v1/sieve-scripts" \
    -H "Content-Type: application/json" \
    -d '{"name":"smoke-test","script":"require [\"fileinto\"];\nif header :contains \"subject\" \"smoke\" { fileinto \"Junk\"; }"}'
body_of /tmp/sv-create.json | tee -a "$OUT"
head -5 /tmp/sv-create-h.txt | tee -a "$OUT"
SCRIPT_ID=$(jq -r '.id // ._id // .name // empty' /tmp/sv-create.json)
echo "SCRIPT_ID=$SCRIPT_ID" | tee -a "$OUT"
if [ -n "$SCRIPT_ID" ]; then
    call /tmp/sv-d.json /tmp/sv-d-h.txt -X DELETE "$BASE/v1/sieve-scripts/$SCRIPT_ID"
    head -3 /tmp/sv-d-h.txt | tee -a "$OUT"
fi

say "Q10: Create a message (IMAP APPEND equivalent)"
call /tmp/msg-create.json /tmp/msg-create-h.txt \
    -X POST "$BASE/v1/messages" \
    -H "Content-Type: application/json" \
    -d '{"folder":"INBOX","raw":"From: smoke@example.com\r\nTo: dotfiles_mcp_test@purpose.dev\r\nSubject: smoke test\r\nDate: Sun, 05 Apr 2026 00:00:00 +0000\r\nMessage-ID: <smoke-test-1@example.com>\r\n\r\nBody text."}'
body_of /tmp/msg-create.json | tee -a "$OUT"
head -10 /tmp/msg-create-h.txt | tee -a "$OUT"
MSG_ID=$(jq -r '.id // ._id // empty' /tmp/msg-create.json)
echo "MSG_ID=$MSG_ID" | tee -a "$OUT"

if [ -n "$MSG_ID" ]; then
    # Try updating flags
    call /tmp/msg-u.json /tmp/msg-u-h.txt \
        -X PUT "$BASE/v1/messages/$MSG_ID" \
        -H "Content-Type: application/json" \
        -d '{"flags":["\\Seen","\\Flagged"]}'
    echo "--- flag update ---" | tee -a "$OUT"
    head -3 /tmp/msg-u-h.txt | tee -a "$OUT"
    body_of /tmp/msg-u.json | tee -a "$OUT"

    # Try updating body (expect failure or silent ignore)
    call /tmp/msg-u2.json /tmp/msg-u2-h.txt \
        -X PUT "$BASE/v1/messages/$MSG_ID" \
        -H "Content-Type: application/json" \
        -d '{"raw":"From: x\r\nSubject: rewritten\r\n\r\nrewritten body"}'
    echo "--- body rewrite attempt ---" | tee -a "$OUT"
    head -3 /tmp/msg-u2-h.txt | tee -a "$OUT"
    body_of /tmp/msg-u2.json | tee -a "$OUT"

    # Re-fetch and see what the state is
    call /tmp/msg-g.json /tmp/msg-g-h.txt "$BASE/v1/messages/$MSG_ID"
    echo "--- post-update fetch ---" | tee -a "$OUT"
    body_of /tmp/msg-g.json | tee -a "$OUT"

    call /tmp/msg-d.json /tmp/msg-d-h.txt -X DELETE "$BASE/v1/messages/$MSG_ID"
    head -3 /tmp/msg-d-h.txt | tee -a "$OUT"
fi

echo "=== PASS 2 DONE ==="
