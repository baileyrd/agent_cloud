#!/bin/sh
# After a host reboot: the units, firewall and agents came back as expected, and agent a1
# resumes through the gateway with a working login and egress (ADR-007), runs a job through the
# task API (ADR-010) with a throwaway jobs-only token that cannot pause it (ADR-011), and leaves
# no zombies (tini is PID 1). Run as root:
#   sudo scripts/post-reboot-check.sh
# Leaves the system as found: if it resumed a1, it pauses it again (KEEP_A1=1 leaves it running);
# the throwaway token is revoked on every way out.
set -u
fail=0
pass() { printf 'PASS  %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; fail=1; }
check() { desc=$1; shift; if "$@" >/dev/null 2>&1; then pass "$desc"; else bad "$desc"; fi; }

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo $0)" >&2; exit 2; }

ENV=/etc/agent-cloud/gateway.env
ALLOW=/etc/agent-cloud/egress-allow.txt
LEASES=/var/lib/cni/networks/agents
NS=agents
AGENT=a1
LAN_HOST=192.168.50.1

# --- host services --------------------------------------------------------------------------
for u in ac-agentfw ac-egress ac-gateway; do check "$u active" systemctl is-active --quiet "$u"; done

# Rules in one chain (lines with a handle, minus the table and chain lines).
rules() { nft -a list chain "$@" 2>/dev/null | grep '# handle' | grep -cvE '^\s*(table|chain) '; }
for spec in "inet agentfw input 3" "inet agentfw forward 3" "bridge agentfw forward 1"; do
    set -- $spec
    n=$(rules "$1" "$2" "$3")
    if [ "$n" = "$4" ]; then pass "nft $1 $2 $3: $n rules"; else bad "nft $1 $2 $3: ${n:-no chain}, want $4"; fi
done

zone="$(firewall-cmd --permanent --zone=agents --list-interfaces 2>&1)|$(firewall-cmd --permanent --zone=agents --list-ports 2>&1)|$(firewall-cmd --permanent --zone=agents --list-services 2>&1)"
if [ "$zone" = "acbr0|3128/tcp|" ]; then pass "firewalld zone agents: acbr0, 3128/tcp only"; else bad "firewalld zone agents: $zone"; fi

check "$ALLOW is root:root 0600" test "$(stat -c '%U:%G %a' "$ALLOW" 2>/dev/null)" = "root:root 600"

# Every listening socket of process $1 is on a private address; prints them.
private_binds() {
    addrs=$(ss -Htlnp | awk -v p="((\"$1\"," 'index($0, p) { print $4 }')
    echo "${addrs:-none}"
    [ -n "$addrs" ] || return 1
    for a in $addrs; do
        case $a in
            127.*|10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[01].*) ;;
            100.6[4-9].*|100.[7-9][0-9].*|100.1[01][0-9].*|100.12[0-7].*|\[::1\]:*) ;;
            *) return 1 ;;
        esac
    done
}
binds() {
    if out=$(private_binds "$1"); then pass "$1 bound only to private addresses: $out"; else bad "$1 binds: $out"; fi
}
binds ac-gateway

# --- agents ---------------------------------------------------------------------------------
val() { sed -n "s/^$1=//p" "$ENV" | tail -n 1; }
BIND=$(val AC_BIND)
[ -z "$(val AC_TOKENS)" ] || export AC_TOKENS="$(val AC_TOKENS)" 
# An admin token: the legacy AC_TOKEN_FILE, else the owner token from docs/install.md.
TOKEN_FILE=$(val AC_TOKEN_FILE)
[ -n "$TOKEN_FILE" ] || TOKEN_FILE=/etc/agent-cloud/owner.token
# The token goes to curl on stdin, not on its command line. $3: an optional (constant) JSON body.
api() {
    printf 'Authorization: Bearer %s\n' "$(cat "$TOKEN_FILE")" |
        curl -sS -m 180 -H @- -X "$1" ${3:+--data-binary "$3"} "http://$BIND/api/v1/agents$2"
}

list=$(api GET "") || list=
agents=$(printf '%s' "$list" | grep -o '"id":"[^"]*","state":"[^"]*"' | sed 's/"id":"\([^"]*\)","state":"\([^"]*\)"/\1 \2/')
[ -n "$agents" ] && pass "gateway lists $(echo "$agents" | wc -l) agent(s)" || bad "gateway list: ${list:-no answer}"

leases_of() {
    for f in "$LEASES"/[0-9]*; do
        [ -f "$f" ] && [ "$(head -n 1 "$f" | tr -d '\r')" = "$1" ] && printf '%s ' "${f##*/}"
    done
}
leftovers() {
    id=$1 left=
    [ -e "/run/netns/ac-$id" ] && left="$left netns"
    for p in "[s]andbox-$id," "[c]ontainerd-shim-kata-v2.* -id $id( |\$)" "[s]hared/sandboxes/$id/shared"; do
        pgrep -f "$p" >/dev/null && left="$left kata:$p"
    done
    ctr -n "$NS" task ls 2>/dev/null | awk -v id="$id" '$1 == id' | grep -q . && left="$left task"
    l=$(leases_of "$id"); [ -n "$l" ] && left="$left lease:$l"
    echo "$left"
}
while read -r id state; do
    [ -n "$id" ] || continue
    case $state in
        paused|destroyed) pass "$id is $state" ;;
        *) bad "$id is $state, want paused" ;;
    esac
    [ "$state" = running ] && continue
    left=$(leftovers "$id")
    if [ -z "$left" ]; then pass "$id: no netns, Kata processes, task or lease"; else bad "$id leftovers:$left"; fi
done <<EOF
$agents
EOF
was=$(echo "$agents" | awk -v id="$AGENT" '$1 == id { print $2 }')

# --- resume a1 and test it from inside ------------------------------------------------------
resumed=
restore() {
    if [ -n "$resumed" ] && [ "${KEEP_A1:-0}" != 1 ]; then
        r=$(api POST "/$AGENT/pause?force=true")
        case $r in *'"state":"paused"'*) pass "$AGENT paused again (as found)" ;; *) bad "pause $AGENT: $r" ;; esac
    fi
    resumed=
}
HOLD= HOLDER= TNAME=
release() {
    [ -z "$HOLDER" ] || kill "$HOLDER" 2>/dev/null; [ -z "$HOLD" ] || rm -f "$HOLD"
    [ -z "$TNAME" ] || ac-gateway token revoke "$TNAME" >/dev/null 2>&1; TNAME=
}
trap 'restore; release; exit "$fail"' EXIT
trap 'fail=1; exit' INT TERM

case $was in
    paused)
        r=$(api POST "/$AGENT/resume")
        case $r in
            *'"state":"running"'*) resumed=1; pass "resume $AGENT through the gateway" ;;
            *) bad "resume $AGENT: ${r:-no answer}"; exit "$fail" ;;
        esac ;;
    running) echo "info  $AGENT was already running; testing it as is" ;;
    *) bad "$AGENT not resumable (state: ${was:-unknown})"; exit "$fail" ;;
esac

# Stdin for execs: a FIFO a background sleep holds open, so no exec ever sees stdin EOF while it
# runs. EOF on a live exec's stdin wedges the Kata shim, and /dev/null is no exception (ADR-010).
HOLD=$(mktemp -u /tmp/ac-check-stdin.XXXXXX) && mkfifo -m 600 "$HOLD" || exit 2
sleep 100000 > "$HOLD" &
HOLDER=$!
# Run a shell command in the agent as its user, with the image's environment.
gx() { ctr -n "$NS" task exec --exec-id "chk-$1-$$" "$AGENT" sh -c "$2" <"$HOLD" 2>&1; }
i=0
until gx ready true >/dev/null; do
    i=$((i + 1)); [ "$i" -lt 30 ] || { bad "$AGENT: exec never worked"; exit "$fail"; }
    sleep 1
done

z=$(firewall-cmd --get-zone-of-interface=acbr0 2>&1)
[ "$z" = agents ] && pass "acbr0 is in firewalld zone agents" || bad "acbr0 zone: $z"
i=0
until private_binds ac-egress >/dev/null || [ "$i" -ge 5 ]; do i=$((i + 1)); sleep 1; done
binds ac-egress

out=$(gx auth 'claude auth status')
case $out in
    *'"loggedIn": true'*|*'"loggedIn":true'*) pass "claude auth status: loggedIn" ;;
    *) bad "claude auth status: $(echo "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac

out=$(gx prompt 'timeout 120 claude -p "say ok" </dev/null')
rc=$?
if [ "$rc" -eq 0 ] && [ -n "$out" ]; then pass "claude -p answers: $(echo "$out" | head -n 1 | cut -c1-80)"
else bad "claude -p (rc $rc): $(echo "$out" | tr '\n' ' ' | cut -c1-200)"; fi

# Through the proxy the first status line is the proxy's CONNECT reply; the second is the API's.
out=$(gx api 'curl -sS -m 8 -I https://api.anthropic.com')
st=$(echo "$out" | grep '^HTTP/' | sed -n 2p | tr -d '\r')
[ -n "$st" ] && pass "api.anthropic.com via proxy: $st" || bad "api.anthropic.com: $(echo "$out" | head -n 3 | tr '\n' ' ')"

out=$(gx deny 'curl -sS -m 8 -I https://example.com')
case $out in
    *'CONNECT tunnel failed, response 403'*) pass "example.com denied by the proxy (403)" ;;
    *) bad "example.com not denied: $(echo "$out" | head -n 3 | tr '\n' ' ')" ;;
esac

out=$(gx lan "curl -sS -m 5 --noproxy '*' -o /dev/null http://$LAN_HOST/ && echo REACHED direct; curl -sS -f -m 5 -o /dev/null http://$LAN_HOST/ && echo REACHED proxied; true")
case $out in
    *REACHED*) bad "$LAN_HOST reachable: $(echo "$out" | tr '\n' ' ')" ;;
    *) pass "$LAN_HOST unreachable, direct and via the proxy" ;;
esac

# --- task API: one job end to end, submitted with a throwaway jobs-only token -----------------
# The token goes to curl on stdin. tok <method> <path> [body]: prints "<status> <body>".
tok() {
    printf 'Authorization: Bearer %s\n' "$TOKEN" |
        curl -sS -m 60 -H @- -X "$1" ${3:+--data-binary "$3"} -w ' %{http_code}' "http://$BIND/api/v1/agents$2" |
        sed 's/^\(.*\) \([0-9]*\)$/\2 \1/'
}
TNAME=post-reboot-check-$$
TOKEN=$(ac-gateway token create "$TNAME" --scopes jobs:read,jobs:write --agents "$AGENT" 2>/dev/null) || TOKEN=
if [ -n "$TOKEN" ]; then pass "token $TNAME created (jobs:read,jobs:write for $AGENT)"; else bad "token create"; TNAME=; fi
sleep 2  # the gateway re-reads the tokens file within a second of a change
st() { sed -n 's/.*"state":"\([a-z_]*\)".*/\1/p'; }
r=$(tok POST "/$AGENT/jobs" '{"prompt":"reply with exactly the word pong","timeout_secs":180}')
case $r in 202\ *) pass "jobs token: job POST 202" ;; *) bad "jobs token: job POST: $r" ;; esac
r=${r#* }
job=$(printf '%s' "$r" | sed -n 's/.*"job_id":"\([a-z0-9-]*\)".*/\1/p')
for spec in "403 POST /$AGENT/pause" "403 POST /$AGENT/term/0011223344556677/takeover" "404 GET /a2/jobs"; do
    set -- $spec
    c=$(tok "$2" "$3" | cut -d' ' -f1)
    if [ "$c" = "$1" ]; then pass "jobs token: $2 $3 -> $c"; else bad "jobs token: $2 $3 -> $c, want $1"; fi
done
c=$(tok GET "/$AGENT/jobs/$job" | cut -d' ' -f1)
[ "$c" = 200 ] && pass "jobs token: GET its job -> 200" || bad "jobs token: GET job -> $c"
if [ -n "$TNAME" ] && ac-gateway token revoke "$TNAME" >/dev/null 2>&1; then
    TNAME=
    sleep 2
    c=$(tok GET "/$AGENT/jobs/$job" | cut -d' ' -f1)
    [ "$c" = 401 ] && pass "revoked token refused (401)" || bad "revoked token still answers $c"
fi
if [ -z "$job" ]; then
    bad "job submit: ${r:-no answer}"
else
    state=$(printf '%s' "$r" | st) i=0
    while { [ "$state" = queued ] || [ "$state" = running ]; } && [ "$i" -lt 8 ]; do
        state=$(api GET "/$AGENT/jobs/$job?wait=30" | st)
        i=$((i + 1))
    done
    out=$(api GET "/$AGENT/jobs/$job/output")
    case "$state:$out" in
        succeeded:*[Pp]ong*) pass "job $job succeeded: $(echo "$out" | head -n 1 | cut -c1-40)" ;;
        *) bad "job $job ${state:-?}: $(echo "$out" | head -c 200 | tr '\n' ' ')" ;;
    esac
fi

# --- no zombies: tini is PID 1 and reaps orphans ---------------------------------------------
out=$(gx init 'ps -o comm= -p 1; ps -eo pid=,ppid=,stat=,comm= | awk "\$2 == 1 && \$3 ~ /^Z/" | wc -l')
init=$(echo "$out" | sed -n 1p) orphans=$(echo "$out" | sed -n 2p | tr -d ' ')
[ "$init" = tini ] && pass "$AGENT PID 1 is tini" || bad "$AGENT PID 1 is ${init:-?}, want tini (rebuild the image, pause/resume)"
[ "$orphans" = 0 ] && pass "$AGENT: no zombies left to PID 1" || bad "$AGENT: $orphans zombie(s) under PID 1"

exit "$fail"
