//! Purge side: resolve a tag to cache keys and invalidate each entry.
//!
//! Invalidation mirrors what NGINX itself does when it drops an entry: the
//! node stays in the cache rbtree, `exists` is cleared and its size is
//! returned to the zone's accounting.  Removing the node outright would race
//! with requests still holding a reference to it.

use core::cmp::Ordering;
use core::ffi::c_char;

use nginx_sys::{
    ngx_create_hashed_filename, ngx_http_file_cache_node_t, ngx_http_file_cache_t,
    ngx_http_request_t, ngx_int_t, ngx_shmtx_lock, ngx_shmtx_unlock,
};
use ngx::core::{SlabPool, Status};
use ngx::http::{HTTPStatus, HttpModuleLocationConf, HttpModuleMainConf, Request};

use xkey_core::{MAX_TAG_LEN, format_usize, hex_encode, node_key, node_key_rest};

use crate::{CACHE_KEY_LEN, CacheKey, HttpXkeyModule, scan};

/// Request header naming the tag to purge.
const PURGE_HEADER: &[u8] = b"xkey-purge";

/// The only method a purge endpoint answers.
///
/// NGINX does not know `PURGE`, so it parses as an unknown method and the
/// literal is kept in `method_name`.
const PURGE_METHOD: &[u8] = b"PURGE";

/// Content handler installed by `xkey_purge`.
pub unsafe extern "C" fn handler(r: *mut ngx_http_request_t) -> ngx_int_t {
    let request = unsafe { Request::from_ngx_http_request(r) };

    if !is_purge(request) {
        request.add_header_out("allow", "PURGE");
        return HTTPStatus::NOT_ALLOWED.into();
    }

    let rc = request.discard_request_body();
    if rc != Status::NGX_OK {
        return rc.into();
    }

    // Copied out of the request so the tag does not keep it borrowed for the
    // rest of the purge. Over-long tags are refused rather than truncated.
    let mut buf = [0u8; MAX_TAG_LEN];
    let Some(tag_len) = purge_tag(request).and_then(|tag| {
        (tag.len() <= MAX_TAG_LEN).then(|| {
            buf[..tag.len()].copy_from_slice(tag);
            tag.len()
        })
    }) else {
        return HTTPStatus::BAD_REQUEST.into();
    };
    let tag = &buf[..tag_len];

    let Some(xlcf) = HttpXkeyModule::location_conf(request.as_ref()) else {
        return Status::NGX_ERROR.into();
    };
    let Some(mut cache) = crate::file_cache(xlcf.cache_zone) else {
        return Status::NGX_ERROR.into();
    };

    let Some(shared) = crate::index_for(request.as_ref()) else {
        return Status::NGX_ERROR.into();
    };

    // Take the whole set under one write lock, then release it before doing
    // any filesystem work.
    let (keys, pressure) = {
        let mut idx = shared.write();
        (
            idx.take(tag),
            Pressure {
                evicted: idx.evicted,
                dropped: idx.dropped,
            },
        )
    };

    let Some(keys) = keys else {
        return fall_back(request, cache.as_ptr(), tag);
    };

    let mut purged = 0usize;
    for key in keys.iter() {
        let cleared = unsafe { invalidate(cache.as_mut(), key) };
        let unlinked = unsafe { delete_file(request, cache.as_mut(), key) };

        if cleared || unlinked {
            purged += 1;
        }
    }

    report(request, "index", purged, None, pressure);
    no_content(request)
}

/// How much the index has had to discard, over the life of the zone.
///
/// Reported only when non-zero, so that a healthy purge stays quiet and a
/// zone that is too small for its traffic says so.
#[derive(Clone, Copy, Default)]
struct Pressure {
    /// Tags evicted to make room.
    evicted: u64,
    /// Keys dropped because a tag reached its cap.
    dropped: u64,
}

/// Answers from the cache directory when the index has nothing for the tag.
///
/// The index can be cold after a restart, or have dropped the tag under
/// pressure. Neither is distinguishable from a tag that was never cached, so
/// the only honest answer is to consult the record on disk.
fn fall_back(request: &mut Request, cache: *mut ngx_http_file_cache_t, tag: &[u8]) -> ngx_int_t {
    let Some(xmcf) = HttpXkeyModule::main_conf_mut(request.as_ref()) else {
        return Status::NGX_ERROR.into();
    };
    let Some(xlcf) = HttpXkeyModule::location_conf(request.as_ref()) else {
        return Status::NGX_ERROR.into();
    };

    if xlcf.fallback == 0 {
        return HTTPStatus::NOT_FOUND.into();
    }

    let buf = request.pool().alloc_unaligned(scan::MAX_HEADER_BLOCK);
    if buf.is_null() {
        return Status::NGX_ERROR.into();
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), scan::MAX_HEADER_BLOCK) };

    // The scan reads every file's tags on its way past, so hand it the index
    // to write them back into. The next purge then answers from memory.
    let index = unsafe { xmcf.shm_zone.as_mut() }.and_then(|zone| {
        let shared = crate::index::shared(zone).ok()?;
        let alloc = unsafe { SlabPool::from_shm_zone(zone) }?;
        Some((shared, alloc))
    });

    let mut s = scan::Scan {
        tag,
        header: xmcf.header.as_bytes(),
        cache,
        buf,
        index,
        scanned: 0,
        purged: 0,
        indexed: 0,
    };

    if scan::run(&mut s, request.log()) == Status::NGX_ERROR {
        return Status::NGX_ERROR.into();
    }

    let scanned = Some((s.scanned, s.indexed));
    let pressure = s
        .index
        .map(|(shared, _)| {
            let idx = shared.read();
            Pressure {
                evicted: idx.evicted,
                dropped: idx.dropped,
            }
        })
        .unwrap_or_default();

    if s.purged == 0 {
        report(request, "scan", 0, scanned, pressure);
        return HTTPStatus::NOT_FOUND.into();
    }

    report(request, "scan", s.purged, scanned, pressure);
    no_content(request)
}

/// Reports what the purge did and how it found out.
fn report(
    request: &mut Request,
    source: &str,
    purged: usize,
    scanned: Option<(usize, usize)>,
    pressure: Pressure,
) {
    request.add_header_out("x-purge-source", source);
    count(request, "x-purged-count", purged);

    if let Some((scanned, indexed)) = scanned {
        count(request, "x-scanned-files", scanned);
        count(request, "x-indexed-files", indexed);
    }

    if pressure.evicted != 0 {
        count(request, "x-index-evictions", pressure.evicted as usize);
    }

    if pressure.dropped != 0 {
        count(request, "x-index-drops", pressure.dropped as usize);
    }
}

fn count(request: &mut Request, name: &str, n: usize) {
    let mut buf = [0u8; 20];
    if let Ok(value) = core::str::from_utf8(format_usize(&mut buf, n)) {
        request.add_header_out(name, value);
    }
}

/// Whether the request uses the PURGE method.
fn is_purge(request: &Request) -> bool {
    request.as_ref().method_name.as_bytes() == PURGE_METHOD
}

/// The tag named by the request, if any.
fn purge_tag(request: &Request) -> Option<&[u8]> {
    for (name, value) in request.headers_in_iterator() {
        if name.as_bytes().eq_ignore_ascii_case(PURGE_HEADER) {
            let value = value.as_bytes();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Clears one cache node's shared-memory accounting.
///
/// Returns whether a live entry was found.  The rbtree is ordered on the full
/// 16-byte key: the node key is the leading `sizeof(ngx_rbtree_key_t)` bytes
/// copied verbatim, in native byte order, with the remainder held in the
/// node's own `key` field and compared bytewise on a tie.
pub(crate) unsafe fn invalidate(cache: &mut ngx_http_file_cache_t, key: &CacheKey) -> bool {
    let node_key = node_key(key);
    let rest = node_key_rest(key);

    let shpool = cache.shpool;
    unsafe { ngx_shmtx_lock(&raw mut (*shpool).mutex) };

    let sh = unsafe { &mut *cache.sh };
    let sentinel = sh.rbtree.sentinel;
    let mut node = sh.rbtree.root;
    let mut found = false;

    while node != sentinel {
        let n = unsafe { &mut *node };

        if node_key < n.key {
            node = n.left;
            continue;
        }
        if node_key > n.key {
            node = n.right;
            continue;
        }

        // The node is the first member of ngx_http_file_cache_node_t.
        let fcn = unsafe { &mut *node.cast::<ngx_http_file_cache_node_t>() };

        match rest.cmp(&fcn.key[..rest.len()]) {
            Ordering::Less => node = n.left,
            Ordering::Greater => node = n.right,
            Ordering::Equal => {
                if fcn.exists() != 0 {
                    sh.size -= fcn.fs_size;
                    fcn.fs_size = 0;
                    fcn.set_exists(0);
                    found = true;
                }
                break;
            }
        }
    }

    unsafe { ngx_shmtx_unlock(&raw mut (*shpool).mutex) };

    found
}

/// Unlinks the cache file backing `key`.
unsafe fn delete_file(
    request: &mut Request,
    cache: &mut ngx_http_file_cache_t,
    key: &CacheKey,
) -> bool {
    let Some(path) = (unsafe { cache.path.as_mut() }) else {
        return false;
    };

    let len = path.name.len + 1 + path.len + 2 * CACHE_KEY_LEN;
    let name = request.pool().alloc_unaligned(len + 1);
    if name.is_null() {
        return false;
    }
    let name = name.cast::<u8>();

    unsafe {
        core::ptr::copy_nonoverlapping(path.name.data, name, path.name.len);

        let hex = core::slice::from_raw_parts_mut(
            name.add(path.name.len + 1 + path.len),
            2 * CACHE_KEY_LEN,
        );
        hex_encode(key, hex).expect("hex buffer sized above");
        *name.add(len) = 0;

        ngx_create_hashed_filename(path, name, len);

        libc::unlink(name.cast::<c_char>()) == 0
    }
}

/// Sends a bodyless 204.  Error statuses are returned to NGINX as a code
/// instead, so that it builds the error response itself.
fn no_content(request: &mut Request) -> ngx_int_t {
    request.set_status(HTTPStatus::NO_CONTENT);
    request.set_content_length_n(0);

    let rc = request.send_header();
    if rc == Status::NGX_ERROR || rc.0 > Status::NGX_OK.0 {
        return rc.into();
    }

    Status::NGX_OK.into()
}
