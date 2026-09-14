//! Status endpoint.
//!
//! Reports what the index holds and what it has had to discard. Without this
//! a zone that is too small degrades quietly: purges keep working, because the
//! scan is authoritative, but they get slower and slower and nothing says why.

use core::fmt::Write;

use nginx_sys::{ngx_chain_t, ngx_http_request_t, ngx_int_t, ngx_pagesize};
use ngx::core::{Buffer, NgxString, Status};
use ngx::http::{HTTPStatus, HttpModuleMainConf, Method, Request};

use crate::{HttpXkeyModule, index};

/// Content handler installed by `xkey_status`.
pub unsafe extern "C" fn handler(r: *mut ngx_http_request_t) -> ngx_int_t {
    let request = unsafe { Request::from_ngx_http_request(r) };

    if !matches!(request.method(), Method::GET | Method::HEAD) {
        request.add_header_out("allow", "GET, HEAD");
        return HTTPStatus::NOT_ALLOWED.into();
    }

    let rc = request.discard_request_body();
    if rc != Status::NGX_OK {
        return rc.into();
    }

    let Some(body) = report(request) else {
        return Status::NGX_ERROR.into();
    };

    request.set_status(HTTPStatus::OK);
    request.set_content_length_n(body.len());
    request.add_header_out("content-type", "application/json");

    let rc = request.send_header();
    if rc == Status::NGX_ERROR || rc.0 > Status::NGX_OK.0 || request.header_only() {
        return rc.into();
    }

    let Ok(body) = core::str::from_utf8(body.as_ref()) else {
        return Status::NGX_ERROR.into();
    };

    send(request, body)
}

/// Renders the index's state as JSON.
fn report(request: &mut Request) -> Option<NgxString<ngx::core::Pool>> {
    // `write!` on an NgxString appends within capacity and never grows it, so
    // the room for the whole document has to be asked for first.
    let mut out = NgxString::new_in(request.pool());
    out.try_reserve(512).ok()?;

    let xmcf = HttpXkeyModule::main_conf_mut(request.as_ref())?;
    let zone = unsafe { xmcf.shm_zone.as_mut() };

    let Some(zone) = zone else {
        // Configured without a zone: say so rather than reporting zeroes that
        // look like an idle index.
        write!(out, "{{\"configured\":false}}\n").ok()?;
        return Some(out);
    };

    let shared = index::shared(zone).ok()?;
    let alloc = unsafe { ngx::core::SlabPool::from_shm_zone(zone) }?;

    // Pages the slab has left, against the region it was given to carve up.
    let (free_pages, total_pages) = {
        let pool = alloc.as_ref();
        let span = pool.end as usize - pool.start as usize;
        (pool.pfree as usize, span / unsafe { ngx_pagesize })
    };

    let snapshot = {
        let idx = shared.read();
        (idx.tags, idx.keys, idx.evicted, idx.dropped)
    };
    let (tags, keys, evictions, key_drops) = snapshot;

    write!(
        out,
        concat!(
            "{{\"configured\":true,",
            "\"zone\":{{\"size\":{},\"pages_total\":{},\"pages_free\":{}}},",
            "\"index\":{{\"tags\":{},\"keys\":{},\"evictions\":{},\"key_drops\":{},",
            "\"max_keys_per_tag\":{}}}}}\n"
        ),
        zone.shm.size,
        total_pages,
        free_pages,
        tags,
        keys,
        evictions,
        key_drops,
        index::MAX_KEYS_PER_TAG,
    )
    .ok()?;

    Some(out)
}

/// Sends `body` as the whole response body.
fn send(request: &mut Request, body: &str) -> ngx_int_t {
    // Copies and, unlike an empty buffer of the right capacity, marks how much
    // of it is filled.
    let Some(mut buf) = request.pool().create_buffer_from_str(body) else {
        return Status::NGX_ERROR.into();
    };

    buf.set_last_buf(request.is_main());
    buf.set_last_in_chain(true);

    let mut chain = ngx_chain_t {
        buf: buf.as_ngx_buf_mut(),
        next: core::ptr::null_mut(),
    };

    request.output_filter(&mut chain).into()
}
