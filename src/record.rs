//! Recording side: a header filter that notes the tags of each cacheable
//! response against the cache key it will be stored under.
//!
//! The filter runs before the entry is committed, so an aborted transfer can
//! leave a tag pointing at a cache key that never materialised.  That is
//! harmless: the purge path simply finds no node for it and moves on.

use nginx_sys::{
    ngx_conf_t, ngx_http_output_header_filter_pt, ngx_http_request_t, ngx_http_top_header_filter,
    ngx_int_t,
};
use ngx::core::{SlabPool, Status};
use ngx::http::{HttpModuleMainConf, Request};

use crate::{CacheKey, HttpXkeyModule, index};

static mut NEXT_HEADER_FILTER: ngx_http_output_header_filter_pt = None;

/// Inserts the recording filter at the top of the header filter chain.
pub unsafe fn install_filter(_cf: *mut ngx_conf_t) -> ngx_int_t {
    unsafe {
        NEXT_HEADER_FILTER = ngx_http_top_header_filter;
        ngx_http_top_header_filter = Some(header_filter);
    }
    Status::NGX_OK.into()
}

unsafe extern "C" fn header_filter(r: *mut ngx_http_request_t) -> ngx_int_t {
    record(unsafe { Request::from_ngx_http_request(r) });

    match unsafe { NEXT_HEADER_FILTER } {
        Some(next) => unsafe { next(r) },
        None => Status::NGX_ERROR.into(),
    }
}

fn record(request: &mut Request) {
    if !request.is_main() {
        return;
    }

    // Only responses on their way into the cache carry a key worth recording.
    let Some(key) = cache_key(request) else {
        return;
    };

    let Some(xmcf) = HttpXkeyModule::main_conf_mut(request.as_ref()) else {
        return;
    };
    let Some(zone) = (unsafe { xmcf.shm_zone.as_mut() }) else {
        return;
    };
    let header_name = xmcf.header.as_bytes();

    let Ok(shared) = index::shared(zone) else {
        return;
    };
    let Some(alloc) = (unsafe { SlabPool::from_shm_zone(zone) }) else {
        return;
    };

    // Collect the header value first; the write lock is held only for the
    // inserts themselves.
    let mut tags: Option<&[u8]> = None;
    for (name, value) in request.headers_out_iterator() {
        if name.as_bytes().eq_ignore_ascii_case(header_name) {
            tags = Some(value.as_bytes());
            break;
        }
    }

    let Some(tags) = tags else {
        return;
    };

    let mut index = shared.write();
    for tag in tags.split(|c| matches!(c, b' ' | b'\t' | b',')) {
        if tag.is_empty() {
            continue;
        }
        if index::insert(&mut index, &alloc, tag, &key).is_err() {
            // The zone is full.  Dropping the association is the honest
            // outcome: a later purge for this tag will miss the entry.
            break;
        }
    }
}

/// The cache key of a response that is being stored, if it is being stored.
fn cache_key(request: &mut Request) -> Option<CacheKey> {
    let upstream = unsafe { request.upstream()?.as_ref() }?;
    if upstream.cacheable() == 0 {
        return None;
    }

    let cache = unsafe { request.as_ref().cache.as_ref() }?;
    if cache.node.is_null() && cache.file.fd == nginx_sys::NGX_INVALID_FILE {
        return None;
    }

    Some(cache.key)
}
