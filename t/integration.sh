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
    curl -sS -m 5 -o /dev/null -w '%{http_code}' \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null
}

purge_count() {
    curl -sS -m 5 -o /dev/null -D - \
        -H "xkey-purge: $1" "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null \
        | grep -i '^x-purged-count:' | tr -d '\r' | awk '{print $2}'
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
    "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' \
        "http://127.0.0.1:$PROXY_PORT/purge" 2>/dev/null)"

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

# Nothing has been served since the restart, so nothing has been re-recorded.
assert_eq "KNOWN GAP: the tag index starts empty after a restart" "404" \
    "$(purge_code all)"

# Serving an entry re-records its tags: NGINX stores the upstream response
# headers in the cache file and replays them on a hit, so the recording
# filter sees the tag header again without any upstream traffic.
assert_eq "NGINX still serves the cached entry" "HIT" "$(get_status /a.html)"
assert_eq "a served entry is re-indexed"        "1"   "$(purge_count all)"

# Entries recorded after the restart are purgeable as usual, so the module
# itself is working; only the pre-restart history is missing.
assert_eq "a post-restart entry is cached" "MISS" "$(get_status /d.html)"
assert_eq "and can be purged by tag"       "1"    "$(purge_count 'page-/d.html')"

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
