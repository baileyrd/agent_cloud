#!/bin/sh
# Run a prompt in an agent through the gateway's task API (ADR-010): submit, wait, print the result.
#   sudo scripts/ac-job.sh a1 'reply with exactly the word pong'
#   sudo scripts/ac-job.sh a1 - < prompt.txt        # prompt from stdin
# Env: TIMEOUT=<secs> (default 1800), KEY=<idempotency key>, STREAM=1 (print the stream log instead).
# The gateway address and token come from /etc/agent-cloud/gateway.env (AC_BIND, AC_TOKEN_FILE),
# or from AC_URL and AC_TOKEN. Progress goes to stderr, the result to stdout.
# Exit: 0 succeeded, 1 the job ended otherwise, 2 usage or API error.
set -u
[ $# -eq 2 ] || { echo "usage: $0 <agent> <prompt | ->" >&2; exit 2; }
agent=$1 prompt=$2
ENV=/etc/agent-cloud/gateway.env
val() { sed -n "s/^$1=//p" "$ENV" 2>/dev/null | tail -n 1; }
url=${AC_URL:-http://$(val AC_BIND)}
token=${AC_TOKEN:-$(cat "$(val AC_TOKEN_FILE)" 2>/dev/null)}
[ -n "$token" ] || { echo "no token: set AC_TOKEN or run as root" >&2; exit 2; }

umask 077
body=$(mktemp) || exit 2
trap 'rm -f "$body"' EXIT

# The token goes to curl on stdin and the body in a 0600 file, so neither is on a command line.
api() {
    printf 'Authorization: Bearer %s\n' "$token" |
        curl -sS -m 60 -H @- -X "$1" ${3:+-H 'Content-Type: application/json' --data-binary "@$3"} "$url/api/v1/agents/$agent$2"
}
field() { sed -n "s/.*\"$1\":\"\\{0,1\\}\\([^\",}]*\\).*/\\1/p"; }

# A JSON string: backslash, quote, tab and CR escaped, lines joined with \n, other control bytes
# dropped. (The API takes any string, \u0000 included; this script handles text.)
json() {
    tr -d '\000-\010\013\014\016-\037' |
        awk 'BEGIN { ORS = ""; print "\"" }
             { if (NR > 1) print "\\n"; gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t");
               gsub(/\r/, "\\r"); print }
             END { print "\"" }'
}
if [ "$prompt" = - ]; then p=$(json); else p=$(printf '%s' "$prompt" | json); fi
printf '{"prompt":%s,"timeout_secs":%s%s}' "$p" "${TIMEOUT:-1800}" \
    "${KEY:+,\"idempotency_key\":\"$KEY\"}" > "$body"

r=$(api POST /jobs "$body") || exit 2
id=$(printf '%s' "$r" | field job_id)
[ -n "$id" ] || { echo "submit: $r" >&2; exit 2; }
state=$(printf '%s' "$r" | field state)
echo "job $id $state" >&2
while [ "$state" = queued ] || [ "$state" = running ]; do
    r=$(api GET "/jobs/$id?wait=30") || exit 2
    new=$(printf '%s' "$r" | field state)
    [ -n "$new" ] || { echo "status: $r" >&2; exit 2; }
    [ "$new" = "$state" ] || echo "job $id $new" >&2
    state=$new
done
reason=$(printf '%s' "$r" | field reason)
[ "$reason" = null ] || echo "reason: $reason" >&2
if [ "${STREAM:-0}" = 1 ]; then api GET "/jobs/$id/output?part=stream"; else api GET "/jobs/$id/output"; echo; fi
[ "$state" = succeeded ]
