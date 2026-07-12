#!/usr/bin/env bash
# Curl-driven system tests against the real stadhouder CGI binaries served
# by the local dev server. Prerequisites:
#   - built binaries staged via copy-to-site.sh
#   - tools/scripts/start-site.sh running  (default http://127.0.0.1:8080;
#     override with the BASE_URL env var)
# The KeyScarf checks (the profile-app smoke check, and the dedicated
# USER_ID_URL identity-verification section near the end) only run if
# KeyScarf is staged (stage-keyscarf.sh); otherwise they're reported as
# skipped, not failed. USER_ID_URL/USER_ID_JSON are temporarily stripped
# from the local cfg for the rest of this suite (restored before that
# section, and on exit either way) so the bulk of these tests - which are
# about stadhouder's own mechanics - don't need a working KeyScarf+DB
# just to run.
#
# Modeled on KeyScarf's system-test-authsite.sh; grows with the engine.
set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."
SITE_ROOT="$REPO_ROOT/site"

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS + 1)); echo "ok       $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL     $1" >&2; }
skip() { SKIP=$((SKIP + 1)); echo "skip     $1"; }

# assert_contains <description> <haystack> <needle>
assert_contains() {
    case "$2" in
        *"$3"*) pass "$1" ;;
        *) fail "$1 - expected to find '$3' in: $2" ;;
    esac
}

# assert_http_status <description> <url> <expected status>
assert_http_status() {
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' "$2")"
    if [ "$code" = "$3" ]; then
        pass "$1"
    else
        fail "$1 - expected HTTP $3 from $2, got $code"
    fi
}

echo "=== stadhouder system tests against $BASE_URL ==="

# send_request <wire-protocol JSON> - one whole client conversation; prints
# the CGI envelope, whose data field is the client program's reply. There
# is one client CGI program for every action - which one is named by
# "kind" in the request body.
send_request() {
    curl -fsS --data "$1" "$BASE_URL/cgi-bin/stadhouder/client"
}

# --- engine: status action ----------------------------------------------------
body="$(send_request '{"kind":"status"}')" \
    || fail "status: client program reachable (is start-site.sh running, and client staged via copy-to-site.sh?)"

if [ -n "$body" ]; then
    assert_contains "status: envelope ok"       "$body" '"ok":true'
    assert_contains "status: service name"      "$body" '"service":"stadhouder"'
    assert_contains "status: version set"       "$body" '"version":"'
    assert_contains "status: reports test_env"  "$body" '"test_env":'
    assert_contains "status: reports time_ms"   "$body" '"time_ms":'
fi

# --- engine: simulated time ---------------------------------------------------
# On a test instance (TEST_ENV=true in the cfg), the engine clock is true
# time + the offset in the state/sim_time_diff file. Verify the current
# offset is applied, then actually travel in time: rewrite the state file,
# check the clock moved, and restore it. Generous +/-60s tolerance: this
# asserts the offset is applied, not that the machine is fast.
CONF="$SITE_ROOT/stadhouder/cfg/stadhouder.conf"
STATE_FILE="$SITE_ROOT/stadhouder/state/sim_time_diff"

# USER_ID_URL/USER_ID_JSON (see the local cfg) make connect verify the
# caller against a real KeyScarf session instead of trusting the
# self-asserted user_id - exercised in its own dedicated section near the
# end of this script. For the bulk of this suite, which is about
# stadhouder's own mechanics and shouldn't need a working KeyScarf+DB just
# to run, that requirement is temporarily lifted (COOKIE_NAME alone stays
# in effect) - restored to the file's exact original contents before this
# script exits, however it exits.
ORIGINAL_CONF_CONTENTS=""
[ -f "$CONF" ] && ORIGINAL_CONF_CONTENTS="$(cat "$CONF")"
restore_conf() {
    [ -n "$ORIGINAL_CONF_CONTENTS" ] && printf '%s\n' "$ORIGINAL_CONF_CONTENTS" > "$CONF"
}
trap restore_conf EXIT

if [ -n "$ORIGINAL_CONF_CONTENTS" ]; then
    grep -viE '^[[:space:]]*USER_ID_(URL|JSON)[[:space:]]*=' "$CONF" > "$CONF.tmp" && mv "$CONF.tmp" "$CONF"
fi

# status_time_ms <description> - the time_ms from a fresh status call.
status_time_ms() {
    send_request '{"kind":"status"}' | sed -nE 's/.*"time_ms":(-?[0-9]+).*/\1/p'
}

# assert_clock_offset <description> <expected offset ms>
assert_clock_offset() {
    local time_ms real_ms delta
    time_ms="$(status_time_ms)"
    real_ms="$(date +%s%3N)"
    if [ -z "$time_ms" ]; then
        fail "$1 - no time_ms in status response"
        return
    fi
    delta=$((time_ms - real_ms - $2))
    if [ "${delta#-}" -lt 60000 ]; then
        pass "$1"
    else
        fail "$1 - expected time_ms ~ real+$2, but it is off by ${delta}ms"
    fi
}

if [ -f "$CONF" ] && grep -qiE '^[[:space:]]*TEST_ENV[[:space:]]*=[[:space:]]*true[[:space:]]*$' "$CONF"; then
    orig_diff="$(cat "$STATE_FILE" 2>/dev/null | tr -d '[:space:]')"
    diff_ms="${orig_diff:-0}"

    assert_clock_offset "sim time: current offset ($diff_ms) applied" "$diff_ms"

    # Time travel: a week beyond the current offset, then restore. The state
    # file is the mechanism tests use to move an instance through time.
    travel_ms=$((diff_ms + 604800000))
    echo "$travel_ms" > "$STATE_FILE"
    assert_clock_offset "sim time: clock follows a state-file rewrite" "$travel_ms"
    if [ -n "$orig_diff" ]; then
        echo "$orig_diff" > "$STATE_FILE"
    else
        rm -f "$STATE_FILE"
    fi
    assert_clock_offset "sim time: original offset restored" "$diff_ms"

    # Sim time can also run at a different rate:
    #   simtime = TIME_FACTOR_START + (caltime - TIME_FACTOR_START) * TIME_FACTOR + SIM_TIME_DIFF
    # With TIME_FACTOR=1000 anchored at "now", one real second advances the
    # sim clock ~1000 seconds.
    sim_state_dir="$(dirname "$STATE_FILE")"
    date +%s%3N > "$sim_state_dir/time_factor_start"
    echo 1000 > "$sim_state_dir/time_factor"
    sleep 1
    time_ms="$(send_request '{"kind":"status"}' | sed -nE 's/.*"time_ms":(-?[0-9]+).*/\1/p')"
    real_ms="$(date +%s%3N)"
    accel=$((time_ms - real_ms - diff_ms))
    if [ -n "$time_ms" ] && [ "$accel" -gt 100000 ]; then
        pass "sim time: TIME_FACTOR accelerates the clock (${accel}ms ahead after ~1s)"
    else
        fail "sim time: expected an accelerated clock, but it is only ${accel:-?}ms ahead"
    fi
    rm -f "$sim_state_dir/time_factor" "$sim_state_dir/time_factor_start"
    assert_clock_offset "sim time: calendar rate restored" "$diff_ms"
else
    skip "sim time: local instance not running TEST_ENV=true (see $CONF)"
fi

# --- engine: service program + client messages ---------------------------------
# The application's service program (here: test-app, the harness's stand-in
# application built on the stadhouder library) is normally started by
# cron-sim (started by start-site.sh, already running in the background,
# firing test-app every simulated minute exactly as real cron would every
# real minute). This script also spawns instances of its own here and
# there, purely so lifecycle tests don't have to wait out cron-sim's own
# cadence - but since cron-sim is always running concurrently, a manually
# spawned instance is NOT guaranteed to be the one that ends up live (it
# may find cron-sim already got there first and defer instantly). So
# liveness/termination are always checked through the service flag file
# and the client protocol - never by tracking a spawned instance's PID.
# Like the CGI binaries, the service finds cfg/ and state/ via
# DOCUMENT_ROOT's parent - passed as a native Windows path under Git Bash
# (cygpath), since a native executable can't resolve MSYS /c/... paths.
SERVICE_BIN=""
for candidate in "$SITE_ROOT/stadhouder/bin/test-app" "$SITE_ROOT/stadhouder/bin/test-app.exe"; do
    [ -f "$candidate" ] && SERVICE_BIN="$candidate"
done

# AUTH_COOKIE - sent on every connect call below, matching the local test
# cfg's COOKIE_NAME=keyscarf_session (see site/stadhouder/cfg/stadhouder.conf).
# Only presence is checked (never the value), so any value does.
AUTH_COOKIE="keyscarf_session=test-harness"

# connect_user <user_id> - a "connect" conversation, carrying the cookie
# connect now requires whenever COOKIE_NAME is configured.
connect_user() {
    curl -fsS -b "$AUTH_COOKIE" --data '{"kind":"connect","user_id":"'"$1"'"}' "$BASE_URL/cgi-bin/stadhouder/client"
}

# send_to <connection id> <message JSON> - a "send" conversation.
send_to() {
    send_request '{"kind":"send","connection_id":"'"$1"'","message":'"$2"'}'
}

# poll_conn <connection id> [wait_ms] - a "poll" conversation. wait_ms
# (engine time) defaults to 0: return immediately with whatever's queued.
# An empty result always triggers a heartbeat send on the way out (see
# client.rs) - that's true here too, at wait_ms=0, just with no blocking.
poll_conn() {
    send_request '{"kind":"poll","connection_id":"'"$1"'","wait_ms":'"${2:-0}"'}'
}

# connection_id_of <reply> - extracts the minted connection id.
connection_id_of() {
    echo "$1" | sed -nE 's/.*"connection_id":"([^"]+)".*/\1/p'
}

# profile_path_of <connection id> - the connection's profile file (uuid
# with underscores for dashes).
profile_path_of() {
    echo "$SITE_ROOT/stadhouder/state/connection_$(echo "$1" | tr '-' '_').json"
}

# poll_until <description> <connection id> <needle> - polls until a batch
# contains the needle. Polls drain the outbox, so per-connection
# expectations must be asserted in arrival order.
poll_until() {
    local tries=0 reply=""
    while [ $tries -lt 30 ]; do
        reply="$(poll_conn "$2")"
        case "$reply" in *"$3"*) pass "$1"; return ;; esac
        tries=$((tries + 1))
        sleep 0.1
    done
    fail "$1 - '$3' never arrived; last reply: $reply"
}

# poll_until_capture <connection id> <needle> - like poll_until but doesn't
# pass/fail itself; returns 0/1 and leaves the matching (or last) batch in
# $POLL_CAPTURED, so multiple substrings can be checked against the SAME
# batch without re-polling (a poll drains the outbox - the message would
# be gone on a second call).
POLL_CAPTURED=""
poll_until_capture() {
    local tries=0 reply=""
    while [ $tries -lt 30 ]; do
        reply="$(poll_conn "$1")"
        case "$reply" in *"$2"*) POLL_CAPTURED="$reply"; return 0 ;; esac
        tries=$((tries + 1))
        sleep 0.1
    done
    POLL_CAPTURED="$reply"
    return 1
}

if [ -z "$SERVICE_BIN" ]; then
    fail "service: test-app not staged in site/stadhouder/bin (run copy-to-site.sh)"
else
    # Leftovers from an aborted earlier run (a killed service can't clean
    # up after itself) would make this run's first instance defer.
    rm -f "$SITE_ROOT/stadhouder/state/service_flag" "$SITE_ROOT/stadhouder/state"/connection_*.json

    doc_root="$(cygpath -w "$SITE_ROOT/public_html" 2>/dev/null || echo "$SITE_ROOT/public_html")"
    SERVICE_FLAG="$SITE_ROOT/stadhouder/state/service_flag"
    DOCUMENT_ROOT="$doc_root" "$SERVICE_BIN" > "$SITE_ROOT/test-app.log" 2>&1 &

    # A live instance exists - whether this is the one that got there (if
    # cron-sim, also running, hadn't already) or it deferred to cron-sim's,
    # doesn't matter: either way the flag now belongs to whichever instance
    # is actually live, and everything past this point talks to that
    # instance purely through the protocol.
    flag_seen=""
    for _ in $(seq 1 30); do
        [ -f "$SERVICE_FLAG" ] && { flag_seen=1; break; }
        sleep 0.1
    done
    if [ -n "$flag_seen" ]; then
        pass "service: a live instance is running (flag file present)"
    else
        fail "service: no service flag file appeared"
    fi

    # init() fired once at startup - a marker file test-app's engine drops
    # purely so this is externally observable (see tools/test-app).
    INIT_MARKER="$SITE_ROOT/stadhouder/state/engine_init_marker"
    if [ -s "$INIT_MARKER" ]; then
        pass "service: engine init() ran (marker file present)"
    else
        fail "service: no engine_init_marker found after startup"
    fi

    # Singleton: a second instance (cron-sim fires every simulated minute,
    # same as real cron every real minute) starts, sees the fresh flag,
    # and shuts back down at once - leaving the already-live instance
    # completely undisturbed.
    flag_before="$(cat "$SERVICE_FLAG" 2>/dev/null)"
    if DOCUMENT_ROOT="$doc_root" timeout 15 "$SERVICE_BIN" > "$SITE_ROOT/test-app-second.log" 2>&1; then
        pass "service: second instance defers to the live one"
    else
        fail "service: second instance did not exit cleanly"
    fi
    assert_contains "service: second instance logs why it deferred" \
        "$(cat "$SITE_ROOT/test-app-second.log" 2>/dev/null)" 'another service instance is operating'
    flag_after="$(cat "$SERVICE_FLAG" 2>/dev/null)"
    if [ "$flag_before" = "$flag_after" ]; then
        pass "service: deferring instance never touched the live flag"
    else
        fail "service: flag changed ($flag_before -> $flag_after) - the deferring instance wrote to it"
    fi
    assert_contains "service: original instance still answers after the deferral" \
        "$(send_request '{"kind":"status"}')" '"ok":true'

    # COOKIE_NAME (see the local cfg) makes connect require a cookie of
    # that name - checked here first, before any connection exists, so a
    # later false positive can't be explained by some other connection's
    # leftover state. Only presence is checked, so a bogus name still
    # counts as present in the second case.
    if [ -f "$CONF" ] && grep -qiE '^[[:space:]]*COOKIE_NAME[[:space:]]*=' "$CONF"; then
        no_cookie_reply="$(curl -fsS --data '{"kind":"connect","user_id":"nocookie"}' "$BASE_URL/cgi-bin/stadhouder/client")"
        assert_contains "service: connect without the required cookie is rejected" \
            "$no_cookie_reply" 'missing required cookie'
        wrong_cookie_reply="$(curl -fsS -b "some_other_cookie=x" --data '{"kind":"connect","user_id":"nocookie"}' "$BASE_URL/cgi-bin/stadhouder/client")"
        assert_contains "service: connect with a different cookie is still rejected" \
            "$wrong_cookie_reply" 'missing required cookie'
        assert_contains "service: connect with the required cookie present succeeds" \
            "$(connect_user cookie_check)" '"connection_id"'
    else
        skip "service: COOKIE_NAME not configured in $CONF - connect-cookie checks skipped"
    fi

    # Connect: the CGI program mints the uuid and writes the connection
    # profile; the service picks it up from the state directory (the
    # greeting arriving proves that whole loop).
    reply="$(connect_user alice)"
    ALICE="$(connection_id_of "$reply")"
    if [ -n "$ALICE" ]; then
        pass "service: connect mints a connection id"
    else
        fail "service: no connection id in connect reply: $reply"
    fi

    profile="$(cat "$(profile_path_of "$ALICE")" 2>/dev/null)"
    if [ -n "$profile" ]; then
        pass "service: connection profile written (uuid underscored in filename)"
    else
        fail "service: no profile file at $(profile_path_of "$ALICE")"
    fi
    assert_contains "service: profile records the user id"       "$profile" '"user_id": "alice"'
    assert_contains "service: profile records the c2s pipe"      "$profile" "c2s_$(echo "$ALICE" | tr '-' '_')"
    assert_contains "service: profile records the s2c pipe"      "$profile" "s2c_$(echo "$ALICE" | tr '-' '_')"
    assert_contains "service: profile records calendar time"     "$profile" '"established_ms"'
    assert_contains "service: profile records simtime (TEST_ENV)" "$profile" '"established_sim_ms"'

    poll_until "service: new connection greeted" "$ALICE" '"hello":"alice"'

    # Blocking poll + heartbeat: an empty poll actually waits close to the
    # requested wait_ms (not returning instantly), and having found
    # nothing, sends a heartbeat that never leaks to the engine as a
    # message (no "unknown command" in the reply).
    poll_start_ms="$(date +%s%3N)"
    empty_reply="$(send_request '{"kind":"poll","connection_id":"'"$ALICE"'","wait_ms":800}')"
    poll_elapsed_ms=$(( $(date +%s%3N) - poll_start_ms ))
    assert_contains "service: empty poll returns an empty array" "$empty_reply" '"messages":[]'
    case "$empty_reply" in
        *"unknown command"*) fail "service: heartbeat leaked to the engine as an unknown command" ;;
        *) pass "service: heartbeat on empty poll is not forwarded to the engine" ;;
    esac
    if [ "$poll_elapsed_ms" -ge 500 ]; then
        pass "service: empty poll actually waits close to wait_ms (${poll_elapsed_ms}ms)"
    else
        fail "service: empty poll returned too fast (${poll_elapsed_ms}ms) - not honoring wait_ms"
    fi

    # A message already on its way: poll with a generous wait_ms returns
    # as soon as it's ready, not after the full window.
    send_to "$ALICE" '{"cmd":"get"}' >/dev/null
    poll_start_ms="$(date +%s%3N)"
    ready_reply="$(send_request '{"kind":"poll","connection_id":"'"$ALICE"'","wait_ms":5000}')"
    poll_elapsed_ms=$(( $(date +%s%3N) - poll_start_ms ))
    assert_contains "service: poll returns a ready message immediately" "$ready_reply" '"counter"'
    if [ "$poll_elapsed_ms" -lt 2000 ]; then
        pass "service: poll with wait_ms=5000 returns early once a message is ready (${poll_elapsed_ms}ms)"
    else
        fail "service: poll took ${poll_elapsed_ms}ms - expected early return, not the full wait_ms"
    fi

    # Messages + state: sends are acknowledged; the engine's replies arrive
    # by poll. One shared counter proves state survives across separate
    # requests and connections.
    assert_contains "service: send acknowledged" "$(send_to "$ALICE" '{"cmd":"add","n":5}')" '"sent":true'
    poll_until "service: message processed (add 5)" "$ALICE" '"counter":5'
    send_to "$ALICE" '{"cmd":"add","n":3}' >/dev/null
    poll_until "service: state survives across requests (add 3)" "$ALICE" '"counter":8'

    reply="$(connect_user bob)"
    BOB="$(connection_id_of "$reply")"
    send_to "$BOB" '{"cmd":"get"}' >/dev/null
    poll_until "service: state shared across connections" "$BOB" '"counter":8'

    # Server push: a broadcast from alice lands in bob's poll.
    send_to "$ALICE" '{"cmd":"broadcast","text":"hi all"}' >/dev/null
    poll_until "service: broadcast reaches another connection" "$BOB" '"broadcast":"hi all"'

    send_to "$ALICE" '{"cmd":"bogus"}' >/dev/null
    poll_until "service: unknown command reported" "$ALICE" 'unknown command'

    # Timers on simulated time: set a 5s timer, jump the sim clock 10s
    # forward, and the timer fires without any real time passing.
    send_to "$ALICE" '{"cmd":"set_timer","delay_ms":5000,"tag":"t1"}' >/dev/null
    poll_until "service: timer set" "$ALICE" '"timer_set":true'
    orig_diff="$(cat "$STATE_FILE" 2>/dev/null | tr -d '[:space:]')"
    echo "$(( ${orig_diff:-0} + 10000 ))" > "$STATE_FILE"
    poll_until "service: timer fired on simulated time" "$ALICE" '"timer_fired":"t1"'
    if [ -n "$orig_diff" ]; then echo "$orig_diff" > "$STATE_FILE"; else rm -f "$STATE_FILE"; fi

    # Timer cancellation: set another timer, cancel it before it's due,
    # jump sim time well past when it WOULD have fired, and confirm it
    # never does.
    send_to "$ALICE" '{"cmd":"set_timer","delay_ms":5000,"tag":"t3"}' >/dev/null
    poll_until "service: cancellable timer set" "$ALICE" '"timer_set":true'
    send_to "$ALICE" '{"cmd":"cancel_timer","tag":"t3"}' >/dev/null
    poll_until "service: cancel acknowledged" "$ALICE" '"timer_cancelled":"t3"'
    orig_diff="$(cat "$STATE_FILE" 2>/dev/null | tr -d '[:space:]')"
    echo "$(( ${orig_diff:-0} + 10000 ))" > "$STATE_FILE"
    sleep 1
    saw_fired=""
    for _ in $(seq 1 5); do
        case "$(poll_conn "$ALICE" 0)" in *'"timer_fired":"t3"'*) saw_fired=1 ;; esac
    done
    if [ -n "$orig_diff" ]; then echo "$orig_diff" > "$STATE_FILE"; else rm -f "$STATE_FILE"; fi
    if [ -z "$saw_fired" ]; then
        pass "service: cancelled timer never fires"
    else
        fail "service: cancelled timer fired anyway"
    fi

    # A faster clock also shortens the service's real sleeps (waits divide
    # by TIME_FACTOR): at factor 20, a 30-second engine timer fires within
    # about 1.5 real seconds.
    sim_state_dir="$(dirname "$STATE_FILE")"
    date +%s%3N > "$sim_state_dir/time_factor_start"
    echo 20 > "$sim_state_dir/time_factor"
    send_to "$ALICE" '{"cmd":"set_timer","delay_ms":30000,"tag":"t2"}' >/dev/null
    poll_until "service: 30s timer fires quickly at TIME_FACTOR=20" "$ALICE" '"timer_fired":"t2"'
    rm -f "$sim_state_dir/time_factor" "$sim_state_dir/time_factor_start"

    body="$(send_request '{"kind":"status"}')"
    assert_contains "service: run visible in status" "$body" '"service_last_run_ms":'

    # --- closes, with reasons ----------------------------------------------------
    # Carol stays connected through the rest of this section - as a
    # witness to the other connections' close-reason broadcasts, and at
    # the end to ask the service to shut down.
    reply="$(connect_user carol)"
    CAROL="$(connection_id_of "$reply")"
    poll_until "service: carol greeted" "$CAROL" '"hello":"carol"'

    # Server side: the engine closes alice itself (close_me) - her profile
    # disappears and later requests see an unknown connection. The runtime
    # can't report a close in the same call that requested it, so this is
    # reported to still-live connections (bob) on the FOLLOWING tick,
    # tagged engine_requested.
    send_to "$ALICE" '{"cmd":"close_me"}' >/dev/null
    gone=""
    for _ in $(seq 1 30); do
        if poll_conn "$ALICE" | grep -q 'unknown connection'; then
            gone=1
            break
        fi
        sleep 0.1
    done
    if [ -n "$gone" ] && [ ! -f "$(profile_path_of "$ALICE")" ]; then
        pass "service: server-side close removes the connection and its profile"
    else
        fail "service: alice still reachable (or profile still present) after close_me"
    fi
    if poll_until_capture "$BOB" "connection_closed"; then
        pass "service: bob is notified of alice's close"
        assert_contains "service: closure reports alice's connection id" "$POLL_CAPTURED" "\"connection_id\":\"$ALICE\""
        assert_contains "service: closure reason is engine_requested" "$POLL_CAPTURED" '"reason":"engine_requested"'
    else
        fail "service: bob was never notified of alice's close"
    fi
    # Carol witnessed the same broadcast (she was connected before alice's
    # close too) - drain it so it doesn't satisfy the "does carol hear
    # about bob's close" check below by coincidence.
    poll_conn "$CAROL" 0 >/dev/null

    # Client side: closing bob deletes his profile, which is how the
    # service learns he is gone - reported to carol as client_disconnected.
    assert_contains "service: client-side close acknowledged" "$(send_request '{"kind":"close","connection_id":"'"$BOB"'"}')" '"closed":true'
    assert_contains "service: closed connection is unknown" "$(poll_conn "$BOB")" 'unknown connection'
    if poll_until_capture "$CAROL" "connection_closed"; then
        pass "service: carol is notified of bob's close"
        assert_contains "service: closure reports bob's connection id" "$POLL_CAPTURED" "\"connection_id\":\"$BOB\""
        assert_contains "service: closure reason is client_disconnected" "$POLL_CAPTURED" '"reason":"client_disconnected"'
    else
        fail "service: carol was never notified of bob's close"
    fi

    # An engine-ordered shutdown ends the application process. Checked two
    # ways, neither involving a PID: the flag disappears (the live
    # instance actually stopped) and carol's own connection - served by
    # that same instance - stops answering.
    send_to "$CAROL" '{"cmd":"shutdown"}' >/dev/null
    ended=""
    for _ in $(seq 1 50); do
        if [ ! -f "$SERVICE_FLAG" ]; then
            ended=1
            break
        fi
        sleep 0.1
    done
    if [ -n "$ended" ]; then
        pass "service: shutdown removed the service flag"
    else
        fail "service: flag still present after shutdown"
    fi
    assert_contains "service: shutdown cleaned the connection up" "$(poll_conn "$CAROL")" '"ok":false'

    # --- lifecycle: per-connection idle timeout + heartbeat keep-alive ----------
    # A short-lived instance of its own, with only erin and frank in it -
    # TIME_FACTOR acceleration is about to fast-forward every connection's
    # inactivity clock, so anything else present (like alice/bob above,
    # whose last real message was a while ago by connection-clock terms)
    # would be collateral damage. Erin gets no traffic at all and should be
    # disconnected after two (simulated) minutes of silence; frank is kept
    # alive purely by its own poll calls' heartbeats - no real application
    # traffic either - proving heartbeats alone are enough to survive the
    # same inactivity window.
    DOCUMENT_ROOT="$doc_root" "$SERVICE_BIN" > "$SITE_ROOT/test-app-idle.log" 2>&1 &
    for _ in $(seq 1 30); do
        [ -f "$SERVICE_FLAG" ] && break
        sleep 0.1
    done

    reply="$(connect_user erin)"
    ERIN="$(connection_id_of "$reply")"
    reply="$(connect_user frank)"
    FRANK="$(connection_id_of "$reply")"
    poll_until "lifecycle: frank greeted" "$FRANK" '"hello":"frank"'
    poll_conn "$ERIN" 0 >/dev/null  # drain erin's own greeting

    sim_state_dir="$(dirname "$STATE_FILE")"
    date +%s%3N > "$sim_state_dir/time_factor_start"
    echo 200 > "$sim_state_dir/time_factor"

    # Each poll is itself a heartbeat when nothing's queued, so simply
    # polling frank on a tight cadence is what keeps him alive; the same
    # loop watches for erin's closure broadcast, checking every reply
    # before it's discarded (a poll drains the outbox - looking afterwards
    # would be too late).
    frank_closed_msg=""
    for _ in $(seq 1 60); do
        reply="$(poll_conn "$FRANK" 0)"
        case "$reply" in *"connection_closed"*) frank_closed_msg="$reply"; break ;; esac
        sleep 0.05
    done
    rm -f "$sim_state_dir/time_factor" "$sim_state_dir/time_factor_start"

    if [ -n "$frank_closed_msg" ]; then
        pass "lifecycle: frank is notified of erin's close"
        assert_contains "lifecycle: closure reports erin's connection id" "$frank_closed_msg" "\"connection_id\":\"$ERIN\""
        assert_contains "lifecycle: closure reason is server_idle_timeout" "$frank_closed_msg" '"reason":"server_idle_timeout"'
    else
        fail "lifecycle: frank was never notified of erin's close"
    fi
    if [ ! -f "$(profile_path_of "$ERIN")" ]; then
        pass "lifecycle: erin (no heartbeats) was disconnected for inactivity"
    else
        fail "lifecycle: erin's profile still present - idle timeout did not fire"
    fi
    if [ -f "$(profile_path_of "$FRANK")" ]; then
        pass "lifecycle: frank (heartbeats only) stayed connected"
    else
        fail "lifecycle: frank was disconnected despite heartbeats"
    fi

    send_to "$FRANK" '{"cmd":"shutdown"}' >/dev/null
    ended=""
    for _ in $(seq 1 50); do
        if [ ! -f "$SERVICE_FLAG" ]; then
            ended=1
            break
        fi
        sleep 0.1
    done
    if [ -n "$ended" ]; then
        pass "lifecycle: idle-test instance's shutdown removed the flag"
    else
        fail "lifecycle: flag still present after idle-test instance's shutdown"
    fi

    # --- lifecycle: a fresh instance takes over ---------------------------------
    # With the flag gone, the next start (cron-sim's next tick, or - to
    # keep this test fast rather than waiting out its cadence - this spawn
    # of our own) becomes the live instance. Snapshot the shutdown marker
    # BEFORE this instance is even started - it's about to become the one
    # whose own idle-exit we're testing below, so "before" must predate
    # its init(), not just predate our later checks (this instance's own
    # shutdown() could otherwise already have run by the time a later
    # snapshot is taken).
    SHUTDOWN_MARKER="$SITE_ROOT/stadhouder/state/engine_shutdown_marker"
    shutdown_marker_before="$(cat "$SHUTDOWN_MARKER" 2>/dev/null)"
    DOCUMENT_ROOT="$doc_root" "$SERVICE_BIN" > "$SITE_ROOT/test-app-b.log" 2>&1 &
    for _ in $(seq 1 30); do
        [ -f "$SERVICE_FLAG" ] && break
        sleep 0.1
    done
    sleep 0.3  # let its startup purge finish before writing a new profile

    reply="$(connect_user dave)"
    DAVE="$(connection_id_of "$reply")"
    poll_until "lifecycle: new instance serves connections" "$DAVE" '"hello":"dave"'

    # Inactivity: jump sim time past the two-minute silence threshold; the
    # service disconnects dave, removing his profile and pipes...
    orig_diff="$(cat "$STATE_FILE" 2>/dev/null | tr -d '[:space:]')"
    echo "$(( ${orig_diff:-0} + 180000 ))" > "$STATE_FILE"
    gone=""
    for _ in $(seq 1 50); do
        if [ ! -f "$(profile_path_of "$DAVE")" ]; then
            gone=1
            break
        fi
        sleep 0.1
    done
    if [ -n "$gone" ]; then
        pass "lifecycle: silent client disconnected after two minutes"
    else
        fail "lifecycle: dave's profile still present after the silence threshold"
    fi

    # ...and, connectionless with its 56-second idle window long past (in
    # engine time), exits on its own and removes its flag - this is the
    # one and only place in this whole run where the engine's shutdown()
    # hook should fire (an idle-exit, as opposed to every OTHER instance
    # here ending via an engine-ordered EngineOutput::shutdown, which
    # doesn't call it) - shutdown_marker_before was captured further up,
    # before this instance was even started.
    ended=""
    for _ in $(seq 1 100); do
        if [ ! -f "$SERVICE_FLAG" ]; then
            ended=1
            break
        fi
        sleep 0.1
    done
    if [ -n "$ended" ]; then
        pass "lifecycle: idle service exited by itself (flag removed)"
    else
        fail "lifecycle: flag still present - service did not idle-exit"
    fi
    shutdown_marker_after="$(cat "$SHUTDOWN_MARKER" 2>/dev/null)"
    if [ -n "$shutdown_marker_after" ] && [ "$shutdown_marker_after" != "$shutdown_marker_before" ]; then
        pass "lifecycle: engine shutdown() ran on idle-exit (marker updated)"
    else
        fail "lifecycle: engine_shutdown_marker was not updated by the idle-exit"
    fi
    if [ -n "$orig_diff" ]; then echo "$orig_diff" > "$STATE_FILE"; else rm -f "$STATE_FILE"; fi

    # --- identity verification: USER_ID_URL / USER_ID_JSON ---------------------
    # Restores the full cfg (USER_ID_URL/USER_ID_JSON included, stripped
    # for the rest of this suite above) and exercises it against a real
    # KeyScarf session. Only meaningful with KeyScarf staged and its DB
    # reachable, so - unlike the rest of this file's connect tests, which
    # only need stadhouder itself - this whole block skips rather than
    # fails when that isn't the case.
    restore_conf
    if [ -f "$SITE_ROOT/public_html/keyscarf/profile.html" ] && grep -qiE '^[[:space:]]*USER_ID_URL[[:space:]]*=' "$CONF"; then
        # keyscarf_login <email> <password> - a real KeyScarf sign-in;
        # prints the resulting keyscarf_session cookie's VALUE (empty on
        # failure). Fixture accounts (alice@example.com etc., password
        # test-password-123 for all of them) come from KeyScarf's own
        # db/test seed data - see authsite/db/test/010_test_users.sql in
        # the KeyScarf repo.
        keyscarf_login() {
            curl -s -D - -o /dev/null --data '{"email":"'"$1"'","password":"'"$2"'"}' \
                "$BASE_URL/cgi-bin/keyscarf/profile_api/api_signin" \
                | tr -d '\r' | sed -nE 's/^[Ss]et-[Cc]ookie: *keyscarf_session=([^;]+).*/\1/p'
        }

        # Negative case first - needs no live stadhouder instance, since an
        # invalid session is rejected by client.exe itself (a call to
        # KeyScarf) before the service is ever involved. A present,
        # correctly-named, but bogus session cookie proves this is genuine
        # authentication, not just COOKIE_NAME's presence check.
        bogus_reply="$(curl -fsS -b "keyscarf_session=not-a-real-session-token" --data '{"kind":"connect","user_id":"eve"}' "$BASE_URL/cgi-bin/stadhouder/client")"
        assert_contains "identity: a present but invalid session is rejected" "$bogus_reply" 'not signed in'

        alice_session="$(keyscarf_login "alice@example.com" "test-password-123")"
        if [ -n "$alice_session" ]; then
            pass "identity: real KeyScarf sign-in obtained a session"

            DOCUMENT_ROOT="$doc_root" "$SERVICE_BIN" > "$SITE_ROOT/test-app-identity.log" 2>&1 &
            for _ in $(seq 1 30); do
                [ -f "$SERVICE_FLAG" ] && break
                sleep 0.1
            done

            # The claimed user_id ("ignored-name") must be discarded - the
            # greeting carries alice's real, stable KeyScarf UUID instead
            # (see authsite/db/test/010_test_users.sql for the fixed id).
            verified_reply="$(curl -fsS -b "keyscarf_session=$alice_session" --data '{"kind":"connect","user_id":"ignored-name"}' "$BASE_URL/cgi-bin/stadhouder/client")"
            VERIFIED_CONN="$(connection_id_of "$verified_reply")"
            if [ -n "$VERIFIED_CONN" ]; then
                pass "identity: connect succeeds with a real session"
                poll_until "identity: greeting carries the verified KeyScarf UUID, not the claimed name" \
                    "$VERIFIED_CONN" '"hello":"00000000-0000-0000-0000-000000000002"'
                send_to "$VERIFIED_CONN" '{"cmd":"shutdown"}' >/dev/null
            else
                fail "identity: connect with a real session failed: $verified_reply"
            fi
        else
            fail "identity: could not sign in to KeyScarf as alice@example.com"
        fi
    else
        skip "identity: USER_ID_URL verification skipped (KeyScarf not staged, or USER_ID_URL not configured in $CONF)"
    fi
fi

# --- identity provider: KeyScarf (when staged) -------------------------------
if [ -f "$SITE_ROOT/public_html/keyscarf/profile.html" ]; then
    assert_http_status "keyscarf: profile app served" "$BASE_URL/keyscarf/profile.html" 200
else
    skip "keyscarf: not staged (run fetch-keyscarf.sh + stage-keyscarf.sh)"
fi

# --- summary ------------------------------------------------------------------
echo "=== $PASS passed, $FAIL failed, $SKIP skipped ==="
[ "$FAIL" -eq 0 ]
