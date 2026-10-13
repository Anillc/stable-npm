#!/usr/bin/env bash
#
# End-to-end check for stable-npm. Needs network access (it talks to the real
# npm registry) and a node/npm on PATH.
#
#   cargo build && scripts/e2e.sh
#
set -euo pipefail

if [[ -n "${NODE_BIN:-}" ]]; then
    export PATH="$NODE_BIN:$PATH"
fi
if ! command -v npm >/dev/null; then
    echo "e2e.sh needs node/npm on PATH (or point NODE_BIN at their bin dir)" >&2
    exit 1
fi

TMP=$(mktemp -d)
export npm_config_cache="$TMP/npm-cache"

BIN=${BIN:-./target/debug/stable-npm}
ADDR=127.0.0.1:3999
BASE="http://$ADDR"
T=2020-01-01T00:00:00Z

pass=0
fail=0

check() { # description expected actual
    if [[ "$2" == "$3" ]]; then
        echo "ok   - $1"
        pass=$((pass + 1))
    else
        echo "FAIL - $1: got '$3', want '$2'"
        fail=$((fail + 1))
    fi
}

start_server() {
    "$BIN" --db "$TMP/stable-npm.db" --addr "$ADDR" --log info \
        >>"$TMP/server.log" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 100); do
        if curl -fsS "$BASE/healthz" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    echo "server did not come up; log:" >&2
    cat "$TMP/server.log" >&2
    return 1
}

stop_server() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        unset SERVER_PID
    fi
}

trap 'stop_server; rm -rf "$TMP"' EXIT

echo "# temp dir: $TMP"
start_server

echo
echo "## packument shape"
curl -fsS "$BASE/$T/express" -o "$TMP/p1.json"
node - "$TMP/p1.json" "$T" <<'EOF'
const fs = require("fs");
const packument = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const t = Date.parse(process.argv[3]);
let bad = 0;
for (const [key, when] of Object.entries(packument.time)) {
    if (key === "created" || key === "modified") continue;
    if (!(key in packument.versions)) {
        console.error("time entry without a version:", key);
        bad++;
    }
    if (Date.parse(when) > t) {
        console.error("time entry after t:", key, when);
        bad++;
    }
}
for (const version of Object.keys(packument.versions)) {
    if (Date.parse(packument.time[version]) > t) {
        console.error("visible version published after t:", version);
        bad++;
    }
}
const latest = packument["dist-tags"].latest;
if (!latest) {
    console.error("no latest tag");
    bad++;
} else if (!(latest in packument.versions)) {
    console.error("latest points at an invisible version:", latest);
    bad++;
} else if (Date.parse(packument.time[latest]) > t) {
    console.error("latest published after t:", latest);
    bad++;
}
console.log(`express versions at ${new Date(t).toISOString()}: ${Object.keys(packument.versions).length}, latest=${latest}`);
process.exit(bad === 0 ? 0 : 1);
EOF
check "packument only contains versions published at or before <time>" yes yes

echo
echo "## determinism"
curl -fsS "$BASE/$T/express" -o "$TMP/p2.json"
cmp -s "$TMP/p1.json" "$TMP/p2.json" && same=yes || same=no
check "repeating the identical request returns identical bytes" yes "$same"

check "expressed with a UTC offset, same instant" yes "$(
    curl -fsS "$BASE/2019-12-31T19:00:00-05:00/express" -o "$TMP/p3.json" &&
        cmp -s "$TMP/p1.json" "$TMP/p3.json" && echo yes || echo no
)"

stop_server
start_server
curl -fsS "$BASE/$T/express" -o "$TMP/p4.json"
cmp -s "$TMP/p1.json" "$TMP/p4.json" && survived=yes || survived=no
check "identical responses survive a restart" yes "$survived"

echo
echo "## boundary handling"
code=$(curl -s -o "$TMP/future.json" -w '%{http_code}' "$BASE/2999-01-01T00:00:00Z/express")
check "future <time> is rejected" 400 "$code"

code=$(curl -s -o "$TMP/ancient.json" -w '%{http_code}' "$BASE/1970-01-01T00:00:00Z/express")
check "before the package existed -> 404" 404 "$code"
check "404 body matches npm" '{"error":"Not found"}' "$(cat "$TMP/ancient.json")"

code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$T/this-package-really-does-not-exist-xyz")
check "missing package -> 404" 404 "$code"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/1609459200000/express")
check "unix timestamps are rejected" 400 "$code"

echo
echo "## scoped packages and single version"
curl -fsS "$BASE/$T/@types%2Fnode" -o "$TMP/scoped.json"
check "scoped packument" "@types/node" "$(node -e "console.log(require(process.argv[1]).name)" "$TMP/scoped.json")"

curl -fsS "$BASE/$T/express/4.17.1" -o "$TMP/one.json"
check "single version manifest" "4.17.1" "$(node -e "console.log(require(process.argv[1]).version)" "$TMP/one.json")"
curl -fsS "$BASE/$T/express/latest" -o "$TMP/tag.json"
check "single version via a dist-tag" "4.17.1" "$(node -e "console.log(require(process.argv[1]).version)" "$TMP/tag.json")"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$T/express/4.18.3")
check "version published after <time> -> 404" 404 "$code"

echo
echo "## audit namespace"
code=$(curl -s -o "$TMP/audit.json" -w '%{http_code}' -X POST "$BASE/$T/-/npm/v1/security/advisories/bulk" -d '{}')
check "advisories/bulk returns empty object" '{}' "$(cat "$TMP/audit.json")"
check "advisories/bulk status" 200 "$code"
curl -fsS -X POST -d '{}' "$BASE/$T/-/npm/v1/security/audits/quick" -o "$TMP/q.json"
node -e '
const r = require(process.argv[1]);
if (r.auditReportVersion !== 2 || Object.keys(r.vulnerabilities).length !== 0) process.exit(1);
' "$TMP/q.json"
check "audits/quick returns an empty report" 0 "$?"

echo
echo "## real npm install"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$T/lodash/-/lodash-4.17.15.tgz")
check "tarball path is a redirect, not a proxy" 302 "$code"
location=$(curl -s -o /dev/null -D - "$BASE/$T/lodash/-/lodash-4.17.15.tgz" | tr -d '\r' | awk 'tolower($1)=="location:"{print $2}')
case "$location" in
https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz) check "tarball redirects to registry.npmjs.org" yes yes ;;
*) check "tarball redirects to registry.npmjs.org" yes "no ($location)" ;;
esac

mkdir -p "$TMP/proj"
pushd "$TMP/proj" >/dev/null
npm init -y >/dev/null 2>&1
npm install --registry "$BASE/$T/" --no-audit --no-fund --ignore-scripts --loglevel=error lodash@4 >/dev/null 2>&1
check "lodash@4 resolved at 2020-01-01" "4.17.15" "$(node -e "console.log(require('lodash/package.json').version)")"
popd >/dev/null

echo
echo "## upstream freshness"
fetches=$(grep -c 'upstream: fetched' "$TMP/server.log" || true)
echo "upstream fetches for the whole run: $fetches"
check "repeat requests did not refetch" yes "$([[ $fetches -le 4 ]] && echo yes || echo no)"

echo
echo "## storage"
db_size() { du -sb "$TMP"/stable-npm.db* 2>/dev/null | awk '{ total += $1 } END { print total + 0 }'; }
before=$(db_size)
for day in $(seq -w 1 20); do
    curl -fsS "$BASE/2019-01-${day}T00:00:00Z/express" -o /dev/null
done
delta=$(( $(db_size) - before ))
echo "storage delta for 20 extra (express, time) requests: ${delta} bytes"
check "extra time points add no per-request bodies" yes \
    "$([[ $delta -lt 2000000 ]] && echo yes || echo no)"
echo "database size: $(du -h "$TMP/stable-npm.db" | cut -f1)"

echo
echo "passed: $pass, failed: $fail"
[[ $fail -eq 0 ]]
