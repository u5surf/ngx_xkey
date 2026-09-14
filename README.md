# ngx_xkey

Surrogate-key (tag) based cache purging for NGINX, in the spirit of Varnish's
`vmod_xkey`. Written in Rust on top of [ngx-rust].

NGINX has no tag-based invalidation. The usual workaround is to encode the
varying dimensions into the cache key and purge with a trailing wildcard, which
walks the whole cache directory and reads every file's header. This module keeps
a tag index in shared memory instead, so a purge costs one lookup plus the
matching entries.

## How it works

An upstream response declares its tags in a header:

```http
xkey: product-1234 category-shoes
```

A header filter records, for every response on its way into the cache, the
association from each tag to that entry's 16-byte cache key. A purge request
naming a tag takes the whole set, clears each node's accounting in the cache
zone and unlinks the file.

The index lives in its own shared memory zone rather than borrowing the cache
zone's slab, so `keys_zone` sizing stays meaningful and an exhausted tag index
cannot starve the cache itself.

## Configuration

```nginx
load_module modules/ngx_http_xkey_module.so;

http {
    proxy_cache_path /var/cache/nginx keys_zone=CACHE:100m levels=1:2;
    xkey_zone xkey:32m;

    server {
        location / {
            proxy_pass  http://backend;
            proxy_cache CACHE;
        }

        location /purge {
            xkey_purge CACHE;
        }
    }
}
```

```console
$ curl -X PURGE -H 'xkey-purge: product-1234' http://localhost/purge
HTTP/1.1 204 No Content
x-purged-count: 3
```

| Directive | Context | Meaning |
|---|---|---|
| `xkey_zone <name>:<size>` | `http` | Shared memory zone holding the tag index |
| `xkey_header <name>` | `http` | Response header listing tags, default `xkey` |
| `xkey_purge <cache_zone>` | `location` | Turn this location into a purge endpoint for the named `proxy_cache_path` zone |
| `xkey_purge_fallback on\|off` | `http`, `server`, `location` | Whether a tag missing from the index falls back to a directory scan. Default `on` |

A purge endpoint answers the `PURGE` method only. Every other method gets
`405` with an `Allow` header and touches nothing, so knowing the endpoint URL
is not enough to empty a cache with an ordinary `GET`.

| Response | Meaning |
|---|---|
| `204` | Tag found. `x-purged-count` names how many live entries were invalidated |
| `404` | The tag is not in the index |
| `400` | No `xkey-purge` header on the request |
| `405` | Not a `PURGE` request |

Every answer says how it was reached. `x-purge-source` is `index` or `scan`; a
scan also reports `x-scanned-files` and `x-indexed-files`.

Restricting the method is a backstop, not access control. The endpoint still
authenticates nobody, so put it behind `allow` / `deny` or an internal
listener.

## The index is an accelerator, not the record

The authoritative copy of an entry's tags is the response header block NGINX
stores inside the cache file. The shared memory index only makes lookups fast.

So when the index has no answer for a tag, the purge does not report a miss. It
walks the cache directory, reads each file's stored headers and purges what
matches. Slow, but always right.

That distinction decides how two otherwise nasty problems behave:

- **A cold index after a restart.** Shared memory does not survive a full stop
  and start, while the cache on disk does. Purges fall back to scanning until
  the index warms up again, which it does as entries are served.
- **Index eviction.** A fixed zone must eventually drop something. Dropping an
  entry costs a scan, never a wrong answer.

A scan reads every surviving file's tags on its way past, so it writes them
back into the index. One scan therefore both answers the request and warms the
index, and a following purge is answered from memory. That is how a cold index
recovers without a separate rebuild pass.

The cost of a scan is one open and one read per cached entry, so it scales with
how many files the cache zone holds, not with how many match. Turning the
fallback off trades that cost for purges that can silently miss.

## Requirements

Tested against these NGINX releases, all passing the full integration suite:

| 1.18.0 | 1.20.2 | 1.22.1 | 1.24.0 | 1.26.3 | 1.28.0 | 1.30.4 |
|---|---|---|---|---|---|---|

The module reads NGINX's file cache structures through bindings generated from
the headers of whatever release it is built against, so nothing about the
layout is hard-coded. It touches no private structure of the proxy module; the
only thing it borrows from there is the module symbol, used as the tag when
looking up a `proxy_cache_path` zone by name.

Older releases may work but are not covered by CI.

## Building

```console
$ cd nginx-source
$ auto/configure --with-compat --add-dynamic-module=/path/to/ngx_xkey
$ make && make install
```

The module is built by cargo through ngx-rust's `auto/rust` integration, so a
Rust toolchain is required at nginx configure time. ngx-rust is pulled from git
at a pinned revision so that builds are reproducible.

## Tests

```console
$ cargo test -p xkey-core      # unit tests, no NGINX needed
$ t/integration.sh             # builds NGINX with the module and exercises it
```

The integration script fetches an NGINX release, builds it with the module,
and runs a real cache through record, purge, reload and error-path scenarios.
Both run in CI on every push.

## Status

Working: tag recording, purge by tag, shared index across workers, index
survival across a reload, lazy re-indexing of entries served after a restart,
the fallback scan and the index warming it performs, `PURGE`-only endpoints,
and reporting which path answered.

Not yet implemented:

- **A bound on the index.** Nothing removes a key except purging its tag, so
  entries for evicted cache files accumulate. Growth is capped by the number of
  distinct cache keys ever seen, which for per-URL tags is the whole URL space.
  A full zone currently drops new associations silently. It needs a cap, LRU
  eviction over tags, and a counter for what it dropped.
- **Soft purge.** Expiring an entry rather than deleting it, so
  `proxy_cache_use_stale` can keep serving while it revalidates.
- **Vary variants.** Variants are stored under a different key; all of them share
  the same stored cache key string, which is what links them.
- **Zone exhaustion policy.** A full tag index currently drops the association
  silently.

[ngx-rust]: https://github.com/nginx/ngx-rust

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

`auto/rust` is copied verbatim from [ngx-rust] and remains under the Apache
License, Version 2.0. See [NOTICE](NOTICE).
