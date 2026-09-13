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

Responses: `204` when the tag was found, with `x-purged-count` naming how many
live entries were invalidated; `404` when the tag is unknown; `400` when the
request carries no `xkey-purge` header.

The purge endpoint has no access control of its own. Put it behind `allow` /
`deny` or an internal listener.

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
`204` / `404` / `400` responses, `x-purged-count`.

Not yet implemented:

- **Index rebuild after a restart.** A full stop and start leaves the cache
  populated but the tag index empty, because NGINX rebuilds its own index from
  cache file names without opening them. Entries are re-indexed as they are
  served, since NGINX replays the stored upstream headers on a hit and the
  recording filter sees the tag header again, so coverage grows with traffic.
  What is missing is indexing entries nobody has requested since the restart: a
  throttled background walk reading each file's stored headers. A reload is
  unaffected, as the zone mapping is reused and the index survives.
- **Tombstones.** While a rebuild is in progress a purge can miss entries not yet
  indexed. Recording purged tags with a timestamp and applying them as the walk
  discovers entries avoids both a retry protocol and an unbounded queue.
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
