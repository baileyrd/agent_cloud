#!/bin/sh
# The launch contract (docs/launch-contract.md, #1066) against the real gateway, plus the checks
# #1067 asks for from inside the VM. As LEL's launcher would, with a throwaway launcher token:
# create an agent with only the `gateway` egress profile, submit a job with a short-lived
# credential, wait for it, read its result and witness, then destroy and purge it. Run as root:
#   sudo scripts/untrusted-run.sh            (AGENT=lel-check by default; KEEP=1 skips the purge)
# Needs /etc/agent-cloud/egress/profiles/gateway.txt (docs/install.md) and a logged-in image login
# on the agent's workspace if the job is to reach the model (with only `gateway` it cannot: the
# job then fails, and the egress and witness checks still run).
set -u
fail=0
pass() { printf 'PASS  %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; fail=1; }

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo $0)" >&2; exit 2; }

ENV=/etc/agent-cloud/gateway.env
PROFILE=/etc/agent-cloud/egress/profiles/gateway.txt
NS=agents
AGENT=${AGENT:-lel-check}
val() { sed -n "s/^$1=//p" "$ENV" | tail -n 1; }
BIND=$(val AC_BIND)
DATA=$(val AC_DATA); DATA=${DATA:-/var/lib/agent-cloud}
[ -z "$(val AC_TOKENS)" ] || export AC_TOKENS="$(val AC_TOKENS)"
[ -n "$BIND" ] || { echo "no AC_BIND in $ENV" >&2; exit 2; }
[ -r "$PROFILE" ] || { echo "no $PROFILE: write the gateway profile first (docs/install.md)" >&2; exit 2; }
ENDPOINT=$(sed -n 's/^endpoint[[:space:]]\{1,\}\([^[:space:]#]*\).*/\1/p' "$PROFILE" | head -n 1)
[ -n "$ENDPOINT" ] || { echo "$PROFILE has no endpoint line" >&2; exit 2; }

TNAME= HOLD= HOLDER= SECRET= created=
cleanup() {
    if [ -n "$created" ] && [ "${KEEP:-0}" != 1 ]; then
        r=$(call DELETE "/$AGENT?purge=true&force=true")
        case $r in 200\ *) pass "destroy and purge $AGENT" ;; *) bad "destroy $AGENT: $r" ;; esac
    fi
    [ -z "$HOLDER" ] || kill "$HOLDER" 2>/dev/null
    [ -z "$HOLD" ] || rm -f "$HOLD"
    [ -z "$SECRET" ] || rm -f "$SECRET"
    [ -z "$TNAME" ] || ac-gateway token revoke "$TNAME" >/dev/null 2>&1
}
trap 'cleanup; exit "$fail"' EXIT
trap 'exit 1' INT TERM

# The token goes to curl on stdin, never on a command line. call <method> <path> [body-file]:
# prints "<status> <body>". A body is read from a file so the credential never reaches an argv.
call() {
    printf 'Authorization: Bearer %s\n' "$TOKEN" |
        curl -sS -m 60 -H @- -X "$1" ${3:+--data-binary "@$3"} -w ' %{http_code}' \
            "http://$BIND/api/v1/agents$2" |
        sed 's/^\(.*\) \([0-9]*\)$/\2 \1/'
}
body() { f=$(mktemp) && printf '%s' "$1" > "$f" && echo "$f"; }
field() { sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\).*/\1/p"; }

# --- 0. a launcher token, as the operator issues one ----------------------------------------
TNAME=untrusted-run-$$
SCOPES=agents:read,agents:write,egress:write,jobs:read,jobs:write
TOKEN=$(ac-gateway token create "$TNAME" --scopes "$SCOPES" --agents "$AGENT" 2>/dev/null) || TOKEN=
[ -n "$TOKEN" ] && pass "launcher token $TNAME ($SCOPES for $AGENT)" || { bad "token create"; TNAME=; exit; }
sleep 2  # the gateway re-reads the tokens file within a second of a change

# --- 1. create with only the gateway profile ------------------------------------------------
f=$(body '{"egress_profiles":["gateway"]}')
r=$(call POST "/$AGENT" "$f"); rm -f "$f"
case $r in 201\ *) pass "create $AGENT with profile gateway"; created=1 ;; *) bad "create: $r"; exit ;; esac
r=$(call GET "/$AGENT/egress")
case $r in *'"profiles":["gateway"]'*) pass "egress: $r" ;; *) bad "egress: $r" ;; esac

# --- inside the VM: only the gateway answers, no secret is there --------------------------
HOLD=$(mktemp -u /tmp/ac-untrusted-stdin.XXXXXX) && mkfifo -m 600 "$HOLD" || exit
sleep 100000 > "$HOLD" &
HOLDER=$!
gx() { ctr -n "$NS" task exec --exec-id "lel-$1-$$" "$AGENT" sh -c "$2" <"$HOLD" 2>&1; }
i=0
until gx ready true >/dev/null; do
    i=$((i + 1)); [ "$i" -lt 30 ] || { bad "$AGENT: exec never worked"; exit; }
    sleep 1
done
connect() { gx "$1" "curl -s -m 8 -o /dev/null -w 'connect=%{http_connect}' $2"; }
out=$(connect gw "https://$ENDPOINT/")
case $out in *connect=200*) pass "gateway endpoint $ENDPOINT: $out" ;; *) bad "gateway endpoint $ENDPOINT: $out" ;; esac
for target in https://api.anthropic.com https://github.com "https://${ENDPOINT%:*}:443"; do
    out=$(connect deny "$target")
    case $out in *connect=200*) bad "$target reachable: $out" ;; *) pass "$target refused: $out" ;; esac
done
out=$(gx direct "curl -sS -m 5 --noproxy '*' -o /dev/null https://1.1.1.1/ && echo REACHED; true")
case $out in *REACHED*) bad "direct IP connect reached 1.1.1.1" ;; *) pass "direct IP connect refused" ;; esac
out=$(gx outside 'touch /etc/lel-escape 2>&1 || echo refused')
case $out in *refused*) pass "write outside /workspace refused" ;; *) bad "write outside /workspace: $out" ;; esac
out=$(gx env 'env | grep -iE "token|secret|api_key|password" | grep -v "^AC_JOB_CREDENTIAL_FILE=" | cut -d= -f1')
[ -z "$out" ] && pass "no secret-looking variables in the agent's environment" || bad "variables: $out"

# --- 2. submit a job with a credential -------------------------------------------------------
CRED="lel.check.$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
SECRET=$(mktemp) && printf '%s\n' "$CRED" > "$SECRET"
f=$(body "{\"prompt\":\"Use the Write tool to create witness-check.txt containing hi. Then reply with exactly: done\",\"timeout_secs\":300,\"idempotency_key\":\"untrusted-run-$$\",\"credential\":\"$CRED\"}")
r=$(call POST "/$AGENT/jobs" "$f"); rm -f "$f"
case $r in 202\ *) pass "job submitted with a credential" ;; *) bad "job POST: $r"; exit ;; esac
JOB=$(printf '%s' "$r" | field job_id)
case $r in *"$CRED"*) bad "the credential is in the submit reply" ;; esac

# --- 3. wait for it --------------------------------------------------------------------------
state=queued i=0
while [ "$i" -lt 12 ]; do
    r=$(call GET "/$AGENT/jobs/$JOB?wait=30")
    state=$(printf '%s' "$r" | field state)
    case $state in queued|running) i=$((i + 1)) ;; *) break ;; esac
done
case $state in
    succeeded) pass "job $JOB succeeded" ;;
    failed|timed_out) echo "info  job $JOB ended $state (expected with only the gateway profile: no model API)" ;;
    *) bad "job $JOB: $state" ;;
esac
out=$(gx cred "ls -d /dev/shm/ac-job-$JOB 2>/dev/null || echo gone")
[ "$out" = gone ] && pass "credential file gone after the job" || bad "credential file left: $out"
if grep -rqF -f "$SECRET" "$DATA/workspaces/$AGENT" "$DATA/jobs/$AGENT" 2>/dev/null; then
    bad "the credential is in the workspace or the job's host files"
else
    pass "the credential is in no workspace or host job file"
fi

# --- 4. result and witness -------------------------------------------------------------------
r=$(call GET "/$AGENT/jobs/$JOB/output")
echo "info  result: $(printf '%s' "${r#* }" | head -c 120)"
i=0
while :; do
    r=$(call GET "/$AGENT/jobs/$JOB/witness")
    case $r in *'changes pending'*) i=$((i + 1)); [ "$i" -lt 30 ] || break; sleep 2 ;; *) break ;; esac
done
case $r in
    200\ *)
        w=$(mktemp)
        printf '%s' "${r#200 }" > "$w"
        check=$(python3 -I -c '
import hashlib, json, sys
w = json.load(open(sys.argv[1]))
ok = hashlib.sha256(w["record"].encode()).hexdigest() == w["sha256"]
print("ok" if ok else "mismatch")
print(w["record"], end="")' "$w")
        rm -f "$w"
        case $check in ok*) pass "witness digest matches its record" ;; *) bad "witness digest: $check" ;; esac
        printf '%s\n' "$check" | sed -n '2,$s/^/      /p'
        printf '%s' "$check" | grep -q "^egress_allow $ENDPOINT " \
            && pass "witness counts the gateway connects" || bad "witness has no egress_allow $ENDPOINT"
        printf '%s' "$check" | grep -q '^egress_deny api.anthropic.com:443 ' \
            && pass "witness counts the refused model API connect" || bad "witness has no egress_deny api.anthropic.com:443"
        ;;
    *) bad "witness: $r" ;;
esac
