#!/bin/sh
#
# Builds NGINX with ngx_xkey as a dynamic module, then exercises the module
# against a real cache: recording tags, purging by tag, and the properties the
# README claims, including that a reload preserves the tag index.
#
# Usage: t/integration.sh [nginx-version]

set -eu

NGINX_VERSION=${1:-${NGINX_VERSION:-1.30.4}}
ADDON_DIR=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ADDON_DIR/target/integration}
ORIGIN_PORT=${ORIGIN_PORT:-18081}
PROXY_PORT=${PROXY_PORT:-18080}

SRC=$WORK/nginx-$NGINX_VERSION
PREFIX=$WORK/inst
CACHE=$WORK/cache

failures=0
checks=0

say() { printf '%s\n' "$*"; }

ok() {
    checks=$((checks + 1))
    say "ok   - $1"
}

fail() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    say "FAIL - $1"
    say "       expected: $2"
    say "       actual:   $3"
}

assert_eq() {
    # assert_eq <description> <expected> <actual>
    if [ "$2" = "$3" ]; then
        ok "$1"
    else
        fail "$1" "$2" "$3"
    fi
}

stop_nginx() {
    if [ -f "$PREFIX/logs/nginx.pid" ]; then
        "$PREFIX/sbin/nginx" -p "$PREFIX" -s stop 2>/dev/null || true
        sleep 1
    fi
}

start_nginx() {
    "$PREFIX/sbin/nginx" -p "$PREFIX"
    sleep 1
}

trap stop_nginx EXIT INT TERM

# --------------------------------------------------------------------------
# Build

mkdir -p "$WORK"

if [ ! -d "$SRC" ]; then
    say "==> fetching nginx $NGINX_VERSION"
    curl -fsSL "https://nginx.org/download/nginx-$NGINX_VERSION.tar.gz" \
        | tar -xzf - -C "$WORK"
fi

say "==> building nginx with ngx_xkey"
cd "$SRC"
./configure \
    --prefix="$PREFIX" \
    --with-compat \
    --without-http_rewrite_module \
    --without-http_gzip_module \
    --add-dynamic-module="$ADDON_DIR" \
    >"$WORK/configure.log" 2>&1 \
    || { cat "$WORK/configure.log"; exit 1; }
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)" \
    >"$WORK/make.log" 2>&1 \
    || { tail -40 "$WORK/make.log"; exit 1; }
make install >/dev/null 2>&1

# --------------------------------------------------------------------------
# Fixture

stop_nginx
rm -rf "$CACHE" "$PREFIX/logs/error.log"
mkdir -p "$CACHE" "$PREFIX/origin" "$PREFIX/logs"

echo one   > "$PREFIX/origin/a.html"
echo two   > "$PREFIX/origin/b.html"
echo three > "$PREFIX/origin/c.html"
echo four  > "$PREFIX/origin/d.html"

cat > "$PREFIX/conf/nginx.conf" <<EOF
load_module modules/ngx_http_xkey_module.so;

worker_processes 2;
daemon on;
error_log logs/error.log info;
pid logs/nginx.pid;

events { worker_connections 64; }

http {
    access_log off;
    proxy_cache_path $CACHE keys_zone=CACHE:10m levels=1:2;
    xkey_zone xkey:4m;

    server {
        listen $ORIGIN_PORT;
        location / {
            root $PREFIX/origin;
            add_header xkey "all page-\$request_uri" always;
        }
    }

    server {
        listen $PROXY_PORT;

        location / {
            proxy_pass http://127.0.0.1:$ORIGIN_PORT;
            proxy_cache CACHE;
            proxy_cache_valid 200 10m;
            add_header X-Cache-Status \$upstream_cache_status always;
        }

        location /purge {
            xkey_purge CACHE;
        }

        location /purge-noscan {
            xkey_purge CACHE;
            xkey_purge_fallback off;
        }
    }
}
EOF

"$PREFIX/sbin/nginx" -p "$PREFIX" -t >/dev/null 2>&1 \
    || { "$PREFIX/sbin/nginx" -p "$PREFIX" -t; exit 1; }

say "==> running"
start_nginx

# --------------------------------------------------------------------------
# Helpers

get_status() {
    curl -sS -m 5 -o /dev/null -D - "http://127.0.0.1:$PROXY_PORT$1" 2>/dev/null \
        | grep -i '^X-Cache-Status:' | tr -d '\r' | awk '{print $2}'
}

purge_code() {
    curl -sS -m 5 -o /dev/null -w '%{http_code}' -X PURGE \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null
}

purge_count() {
    curl -sS -m 5 -o /dev/null -D - -X PURGE \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null \
        | grep -i '^x-purged-count:' | tr -d '\r' | awk '{print $2}'
}

# <method>
method_code() {
    curl -sS -m 5 -o /dev/null -w '%{http_code}' -X "$1" \
        -H "xkey-purge: all" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null
}

# <tag> <header-name>
purge_header() {
    curl -sS -m 5 -o /dev/null -D - -X PURGE \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null \
        | grep -i "^$2:" | tr -d '\r' | awk '{print $2}'
}

noscan_code() {
    curl -sS -m 5 -o /dev/null -w '%{http_code}' -X PURGE \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge-noscan" 2>/dev/null
}

cached_files() {
    find "$CACHE" -type f 2>/dev/null | wc -l | tr -d ' '
}

# --------------------------------------------------------------------------
# Tests

assert_eq "first request misses" "MISS" "$(get_status /a.html)"
assert_eq "second request hits"  "HIT"  "$(get_status /a.html)"
assert_eq "entry is on disk"     "1"    "$(cached_files)"

assert_eq "purging a per-page tag reports one entry" "1" "$(purge_count 'page-/a.html')"
assert_eq "the cache file is gone"                   "0" "$(cached_files)"
assert_eq "the next request misses again"            "MISS" "$(get_status /a.html)"

get_status /a.html >/dev/null
get_status /b.html >/dev/null
get_status /c.html >/dev/null
assert_eq "three entries are cached" "3" "$(cached_files)"

assert_eq "a shared tag purges every entry carrying it" "3" "$(purge_count all)"
assert_eq "all cache files are gone"                    "0" "$(cached_files)"

assert_eq "re-purging a consumed tag is a miss" "404" "$(purge_code all)"
assert_eq "an unknown tag is a miss"            "404" "$(purge_code no-such-tag)"

assert_eq "a request with no tag header is rejected" "400" \
    "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -X PURGE \
        "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null)"

# Only PURGE reaches the purge logic.  A GET must not be able to empty a
# cache just by knowing the endpoint URL.
assert_eq "GET is rejected"    "405" "$(method_code GET)"
assert_eq "POST is rejected"   "405" "$(method_code POST)"
assert_eq "HEAD is rejected"   "405" "$(method_code HEAD)"
assert_eq "DELETE is rejected" "405" "$(method_code DELETE)"

assert_eq "the rejection advertises the allowed method" "PURGE" \
    "$(curl -sS -m 5 -o /dev/null -D - -X GET \
        "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null \
        | grep -i '^allow:' | tr -d '\r' | awk '{print $2}')"

# A rejected GET must leave the cache untouched.
get_status /a.html >/dev/null
assert_eq "a rejected GET purges nothing" "1" "$(cached_files)"
assert_eq "and the entry is still purgeable" "1" "$(purge_count 'page-/a.html')"

# The tag index lives in a zone NGINX reuses across a reload, so associations
# recorded before the reload must still resolve afterwards.
get_status /a.html >/dev/null
get_status /b.html >/dev/null
"$PREFIX/sbin/nginx" -p "$PREFIX" -s reload
sleep 2
assert_eq "the index survives a reload" "2" "$(purge_count all)"
assert_eq "reload-purged files are gone" "0" "$(cached_files)"

# A full restart drops the shared memory holding the tag index, while NGINX
# rebuilds its own cache index from cache file names without opening them.
# The cache therefore survives but the tag index does not.  These assertions
# document that gap; they are what changes when the background rebuild lands.
get_status /a.html >/dev/null
get_status /b.html >/dev/null
assert_eq "two entries cached before the restart" "2" "$(cached_files)"

stop_nginx
start_nginx

assert_eq "cache files survive a restart" "2" "$(cached_files)"

# Nothing has been served since the restart, so the index holds nothing. The
# fallback consults the record on disk instead, and still purges correctly.
assert_eq "a cold index still purges, via the scan" "2" "$(purge_count all)"
assert_eq "the scan emptied the cache"              "0" "$(cached_files)"

# With the fallback disabled, a cold index answers 404 instead of scanning.
get_status /a.html >/dev/null
assert_eq "the entry is cached again" "1" "$(cached_files)"
stop_nginx
start_nginx
assert_eq "fallback off reports a miss on a cold index" "404" "$(noscan_code all)"
assert_eq "and leaves the entry alone"                  "1"   "$(cached_files)"

# Serving an entry re-records its tags: NGINX stores the upstream response
# headers in the cache file and replays them on a hit, so the recording
# filter sees the tag header again without any upstream traffic.
assert_eq "NGINX still serves the cached entry" "HIT" "$(get_status /a.html)"
assert_eq "a served entry is re-indexed"        "1"   "$(purge_count all)"

# Entries recorded after the restart are purgeable as usual, so the module
# itself is working; only the pre-restart history is missing.
assert_eq "a post-restart entry is cached" "MISS" "$(get_status /d.html)"
assert_eq "and can be purged by tag"       "1"    "$(purge_count 'page-/d.html')"

# The response says which path answered, so a miss from a lossy index is not
# mistaken for a tag that was never cached.
get_status /c.html >/dev/null
assert_eq "an index answer says so" "index" "$(purge_header 'page-/c.html' x-purge-source)"

get_status /c.html >/dev/null
stop_nginx
start_nginx
assert_eq "a scan answer says so" "scan" "$(purge_header 'page-/c.html' x-purge-source)"

get_status /c.html >/dev/null
stop_nginx
start_nginx
assert_eq "and reports how many files it read" "1" \
    "$(purge_header 'page-/c.html' x-scanned-files)"

assert_eq "an unknown tag is still a miss after scanning" "404" "$(purge_code no-such-tag)"

long_tag=$(printf 'a%.0s' $(seq 1 1100))
assert_eq "an over-long tag is refused" "400" "$(purge_code "$long_tag")"

errors=$(grep -cE '\[(error|crit|alert|emerg)\]' "$PREFIX/logs/error.log" 2>/dev/null || true)
assert_eq "no errors were logged" "0" "${errors:-0}"

# --------------------------------------------------------------------------

say ""
say "$((checks - failures))/$checks checks passed"

if [ "$failures" -ne 0 ]; then
    say ""
    say "==> error.log"
    tail -40 "$PREFIX/logs/error.log" 2>/dev/null || true
    exit 1
fi
