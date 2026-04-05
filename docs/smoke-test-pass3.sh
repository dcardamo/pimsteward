#!/usr/bin/env bash
# Pass 3: fix field-name issues from pass 2 and probe remaining questions.
set -uo pipefail

USER=$(cat ~/.config/secrets/pimsteward-test-alias-user)
PASS=$(cat ~/.config/secrets/pimsteward-test-alias-password)
BASE="https://api.forwardemail.net"
OUT=/tmp/fe-smoke-findings.md

say() { echo -e "\n\n## PASS 3 — $*\n" | tee -a "$OUT"; }
call() {
    local body_file="$1"; shift
    local headers_file="$1"; shift
    curl -sS -u "$USER:$PASS" -o "$body_file" -D "$headers_file" -w "HTTP %{http_code}\n" "$@"
}
body_of() { jq -C . "$1" 2>/dev/null | head -40; }

say "Q1: Create calendar, then event with multiple field-name attempts"
call /tmp/c.json /tmp/c-h.txt -X POST "$BASE/v1/calendars" -H "Content-Type: application/json" -d '{"name":"smoke3"}'
CAL_ID=$(jq -r '.id // empty' /tmp/c.json)
echo "cal=$CAL_ID" | tee -a "$OUT"

# Attempt 1: calendar_id
echo -e "\n=== attempt: calendar_id field ===" | tee -a "$OUT"
call /tmp/e1.json /tmp/e1-h.txt -X POST "$BASE/v1/calendar-events" -H "Content-Type: application/json" \
    -d "{\"calendar_id\":\"$CAL_ID\",\"summary\":\"t1\",\"dtstart\":\"2027-01-15T10:00:00.000Z\",\"dtend\":\"2027-01-15T11:00:00.000Z\"}"
body_of /tmp/e1.json | tee -a "$OUT"
EID=$(jq -r '.id // empty' /tmp/e1.json)

if [ -z "$EID" ]; then
    # Attempt 2: use calendar path in URL
    echo -e "\n=== attempt: POST to /calendars/:id/events ===" | tee -a "$OUT"
    call /tmp/e2.json /tmp/e2-h.txt -X POST "$BASE/v1/calendars/$CAL_ID/events" -H "Content-Type: application/json" \
        -d '{"summary":"t2","dtstart":"2027-01-15T10:00:00.000Z","dtend":"2027-01-15T11:00:00.000Z"}'
    body_of /tmp/e2.json | tee -a "$OUT"
    EID=$(jq -r '.id // empty' /tmp/e2.json)
fi

if [ -z "$EID" ]; then
    # Attempt 3: raw iCalendar content (mirroring contacts which accept `content`)
    echo -e "\n=== attempt: raw iCalendar content ===" | tee -a "$OUT"
    ICS=$'BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//pimsteward//smoke//EN\nBEGIN:VEVENT\nUID:smoke3-event@pimsteward\nSUMMARY:t3\nDTSTART:20270115T100000Z\nDTEND:20270115T110000Z\nEND:VEVENT\nEND:VCALENDAR'
    PAYLOAD=$(jq -n --arg cal "$CAL_ID" --arg ics "$ICS" '{calendar_id: $cal, content: $ics}')
    call /tmp/e3.json /tmp/e3-h.txt -X POST "$BASE/v1/calendar-events" -H "Content-Type: application/json" -d "$PAYLOAD"
    body_of /tmp/e3.json | tee -a "$OUT"
    EID=$(jq -r '.id // empty' /tmp/e3.json)
fi

if [ -n "$EID" ]; then
    echo -e "\nEVENT ID: $EID" | tee -a "$OUT"
    # Fetch, check content field, ETag header, etag json field
    call /tmp/eg.json /tmp/eg-h.txt "$BASE/v1/calendar-events/$EID"
    echo -e "\n--- event fetch body (head) ---" | tee -a "$OUT"
    body_of /tmp/eg.json | tee -a "$OUT"
    echo -e "\n--- headers ---" | tee -a "$OUT"
    grep -iE '^(etag|last-modified):' /tmp/eg-h.txt | tee -a "$OUT"

    # Churn test
    sleep 2
    call /tmp/eg2.json /tmp/eg2-h.txt "$BASE/v1/calendar-events/$EID"
    if diff -q /tmp/eg.json /tmp/eg2.json > /dev/null; then
        echo "**FINDING:** event bytes stable across GETs" | tee -a "$OUT"
    else
        echo "**FINDING:** event bytes differ:" | tee -a "$OUT"
        diff /tmp/eg.json /tmp/eg2.json | head -20 | tee -a "$OUT"
    fi

    # If-Match round-trip
    ETAG=$(grep -i '^etag:' /tmp/eg-h.txt | awk '{print $2}' | tr -d '\r')
    echo "event ETag: $ETAG" | tee -a "$OUT"

    call /tmp/eu1.json /tmp/eu1-h.txt -X PUT "$BASE/v1/calendar-events/$EID" \
        -H "Content-Type: application/json" \
        -H "If-Match: $ETAG" \
        -d '{"summary":"updated with correct etag"}'
    echo "--- PUT with correct If-Match ---" | tee -a "$OUT"
    head -3 /tmp/eu1-h.txt | tee -a "$OUT"
    body_of /tmp/eu1.json | tee -a "$OUT"

    call /tmp/eu2.json /tmp/eu2-h.txt -X PUT "$BASE/v1/calendar-events/$EID" \
        -H "Content-Type: application/json" \
        -H 'If-Match: "stale-bogus"' \
        -d '{"summary":"should fail"}'
    echo "--- PUT with stale If-Match ---" | tee -a "$OUT"
    head -3 /tmp/eu2-h.txt | tee -a "$OUT"
    body_of /tmp/eu2.json | tee -a "$OUT"

    call /tmp/ed.json /tmp/ed-h.txt -X DELETE "$BASE/v1/calendar-events/$EID"
fi

# Clean up calendar
call /tmp/cd.json /tmp/cd-h.txt -X DELETE "$BASE/v1/calendars/$CAL_ID"

say "Q2: Sieve script — try correct field names"
call /tmp/s1.json /tmp/s1-h.txt -X POST "$BASE/v1/sieve-scripts" -H "Content-Type: application/json" \
    -d '{"name":"smoke3","content":"require [\"fileinto\"];\nif header :contains \"subject\" \"smoke\" { fileinto \"Junk\"; }"}'
body_of /tmp/s1.json | tee -a "$OUT"
SID=$(jq -r '.id // .name // empty' /tmp/s1.json)
if [ -n "$SID" ]; then
    echo "sieve id=$SID" | tee -a "$OUT"
    call /tmp/sg.json /tmp/sg-h.txt "$BASE/v1/sieve-scripts/$SID"
    echo "--- sieve fetch ---" | tee -a "$OUT"
    body_of /tmp/sg.json | tee -a "$OUT"
    grep -iE '^etag:' /tmp/sg-h.txt | tee -a "$OUT"
    call /tmp/sd.json /tmp/sd-h.txt -X DELETE "$BASE/v1/sieve-scripts/$SID"
fi

say "Q3: Contact If-Match behaviour — definitive check"
call /tmp/ct.json /tmp/ct-h.txt -X POST "$BASE/v1/contacts" -H "Content-Type: application/json" \
    -d '{"full_name":"IfMatch Test","emails":[{"type":"work","value":"im@example.com"}]}'
CID=$(jq -r '.id' /tmp/ct.json)
ETAG=$(jq -r '.etag' /tmp/ct.json)
echo "contact id=$CID etag=$ETAG" | tee -a "$OUT"

# Try update WITHOUT If-Match
call /tmp/cu0.json /tmp/cu0-h.txt -X PUT "$BASE/v1/contacts/$CID" -H "Content-Type: application/json" \
    -d '{"full_name":"no if-match"}'
echo "--- PUT without If-Match ---" | tee -a "$OUT"
head -3 /tmp/cu0-h.txt | tee -a "$OUT"
echo "success: $(jq -r '.full_name // .error' /tmp/cu0.json)" | tee -a "$OUT"

# Try update with stale If-Match
call /tmp/cu1.json /tmp/cu1-h.txt -X PUT "$BASE/v1/contacts/$CID" \
    -H "Content-Type: application/json" \
    -H 'If-Match: "definitely-stale"' \
    -d '{"full_name":"with stale if-match"}'
echo "--- PUT with stale If-Match ---" | tee -a "$OUT"
head -3 /tmp/cu1-h.txt | tee -a "$OUT"
echo "result: $(jq -r '.full_name // .error' /tmp/cu1.json)" | tee -a "$OUT"

call /tmp/cd.json /tmp/cd-h.txt -X DELETE "$BASE/v1/contacts/$CID"

say "Q4: Message body rewrite — did pass 2's body PUT actually change anything?"
call /tmp/m.json /tmp/m-h.txt -X POST "$BASE/v1/messages" -H "Content-Type: application/json" \
    -d '{"folder":"INBOX","raw":"From: original@example.com\r\nTo: dotfiles_mcp_test@purpose.dev\r\nSubject: original subject\r\nMessage-ID: <rewrite-test@example.com>\r\n\r\nOriginal body."}'
MID=$(jq -r '.id' /tmp/m.json)
echo "msg id=$MID" | tee -a "$OUT"
if [ -n "$MID" ] && [ "$MID" != "null" ]; then
    # Fetch original
    call /tmp/mg1.json /tmp/mg1-h.txt "$BASE/v1/messages/$MID"
    ORIG_SUB=$(jq -r '.subject' /tmp/mg1.json)
    ORIG_SIZE=$(jq -r '.size' /tmp/mg1.json)
    echo "original subject='$ORIG_SUB' size=$ORIG_SIZE" | tee -a "$OUT"

    # Attempt body rewrite via PUT
    call /tmp/mu.json /tmp/mu-h.txt -X PUT "$BASE/v1/messages/$MID" -H "Content-Type: application/json" \
        -d '{"raw":"From: rewritten@example.com\r\nSubject: REWRITTEN SUBJECT\r\n\r\nREWRITTEN body."}'
    echo "--- PUT with raw ---" | tee -a "$OUT"
    head -3 /tmp/mu-h.txt | tee -a "$OUT"

    # Fetch again — did subject change?
    call /tmp/mg2.json /tmp/mg2-h.txt "$BASE/v1/messages/$MID"
    NEW_SUB=$(jq -r '.subject' /tmp/mg2.json)
    NEW_SIZE=$(jq -r '.size' /tmp/mg2.json)
    echo "after PUT: subject='$NEW_SUB' size=$NEW_SIZE" | tee -a "$OUT"
    if [ "$ORIG_SUB" = "$NEW_SUB" ] && [ "$ORIG_SIZE" = "$NEW_SIZE" ]; then
        echo "**FINDING Q4:** PUT {raw:...} was IGNORED — message body is effectively immutable via REST. Confirms the pimsteward plan assumption." | tee -a "$OUT"
    else
        echo "**FINDING Q4:** PUT {raw:...} CHANGED the message. subject and/or size differs." | tee -a "$OUT"
    fi

    call /tmp/md.json /tmp/md-h.txt -X DELETE "$BASE/v1/messages/$MID"
fi

say "Q5: Search filter support — test 'since' parameter"
# Create a message dated yesterday, one dated today
call /tmp/m1.json _ -X POST "$BASE/v1/messages" -H "Content-Type: application/json" \
    -d '{"folder":"INBOX","raw":"From: a@x.com\r\nTo: dotfiles_mcp_test@purpose.dev\r\nSubject: search-test-A\r\nDate: Fri, 03 Apr 2026 10:00:00 +0000\r\nMessage-ID: <search-a@example.com>\r\n\r\nA."}'
call /tmp/m2.json _ -X POST "$BASE/v1/messages" -H "Content-Type: application/json" \
    -d '{"folder":"INBOX","raw":"From: b@x.com\r\nTo: dotfiles_mcp_test@purpose.dev\r\nSubject: search-test-B\r\nDate: Sat, 04 Apr 2026 10:00:00 +0000\r\nMessage-ID: <search-b@example.com>\r\n\r\nB."}'
M1=$(jq -r '.id' /tmp/m1.json); M2=$(jq -r '.id' /tmp/m2.json)
echo "created m1=$M1 m2=$M2" | tee -a "$OUT"

# Search since 2026-04-04
call /tmp/ms.json _ "$BASE/v1/messages?since=2026-04-04T00:00:00.000Z"
echo "--- search 'since=2026-04-04' ---" | tee -a "$OUT"
jq -C '[.[] | {id, subject, header_date}]' /tmp/ms.json 2>&1 | head -20 | tee -a "$OUT"

# Search with subject filter
call /tmp/ms2.json _ "$BASE/v1/messages?subject=search-test-A"
echo "--- search 'subject=search-test-A' ---" | tee -a "$OUT"
jq -C '[.[] | {id, subject}]' /tmp/ms2.json 2>&1 | head -10 | tee -a "$OUT"

# Cleanup
[ -n "$M1" ] && [ "$M1" != "null" ] && curl -sS -u "$USER:$PASS" -X DELETE "$BASE/v1/messages/$M1" >/dev/null
[ -n "$M2" ] && [ "$M2" != "null" ] && curl -sS -u "$USER:$PASS" -X DELETE "$BASE/v1/messages/$M2" >/dev/null

say "Q6: Final rate-limit state"
curl -sS -u "$USER:$PASS" -D /tmp/final-h.txt -o /dev/null "$BASE/v1/account"
grep -iE 'ratelimit' /tmp/final-h.txt | tee -a "$OUT"

echo "=== PASS 3 DONE ==="
