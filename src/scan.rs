//! Fallback scan of the cache directory.
//!
//! The tag index is an accelerator, not the record. The authoritative copy of
//! an entry's tags is the response header block NGINX stores inside the cache
//! file itself. When the index cannot answer, this walks the cache directory
//! and reads that block, which is slow but always correct.
//!
//! That keeps index eviction and a cold index after a restart as questions of
//! speed rather than of correctness: losing an index entry costs a scan, never
//! a wrong answer.

use core::ffi::{c_char, c_void};
use core::{mem, ptr};

use nginx_sys::{
    NGX_HTTP_CACHE_VERSION, ngx_http_file_cache_header_t, ngx_http_file_cache_t, ngx_int_t,
    ngx_log_t, ngx_str_t, ngx_tree_ctx_t, ngx_walk_tree,
};
use ngx::core::{SlabPool, Status};
use xkey_core::{CACHE_KEY_LEN, CacheKey, find_header, hex_decode, split_tags};

use crate::index::Shared;
use crate::purge;

/// Largest stored header block, bounded by `body_start` being a `u_short`.
pub const MAX_HEADER_BLOCK: usize = u16::MAX as usize;

/// State carried through a walk of one cache directory.
pub struct Scan<'a> {
    /// The tag being purged.
    pub tag: &'a [u8],
    /// Name of the header the tags are stored under.
    pub header: &'a [u8],
    /// Cache whose directory is being walked.
    pub cache: *mut ngx_http_file_cache_t,
    /// Scratch space for one file's header block, reused across the walk.
    pub buf: &'a mut [u8],
    /// Index to write back into, and the allocator backing it.
    pub index: Option<(&'a Shared, SlabPool)>,
    /// Files visited.
    pub scanned: usize,
    /// Entries invalidated.
    pub purged: usize,
    /// Files whose tags were written back into the index.
    pub indexed: usize,
}

/// Walks the cache directory, purging every entry carrying the scan's tag.
pub fn run(scan: &mut Scan, log: *mut ngx_log_t) -> Status {
    let Some(path) = (unsafe { scan.cache.as_mut() }).and_then(|c| unsafe { c.path.as_mut() })
    else {
        return Status::NGX_ERROR;
    };
    let mut name = path.name;

    let mut tree: ngx_tree_ctx_t = unsafe { mem::zeroed() };
    tree.init_handler = None;
    tree.file_handler = Some(on_file);
    tree.pre_tree_handler = Some(on_noop);
    tree.post_tree_handler = Some(on_noop);
    tree.spec_handler = Some(on_noop);
    tree.data = ptr::from_mut(scan).cast::<c_void>();
    tree.alloc = 0;
    tree.log = log;

    Status(unsafe { ngx_walk_tree(&raw mut tree, &raw mut name) } as _)
}

unsafe extern "C" fn on_noop(_ctx: *mut ngx_tree_ctx_t, _path: *mut ngx_str_t) -> ngx_int_t {
    Status::NGX_OK.into()
}

unsafe extern "C" fn on_file(ctx: *mut ngx_tree_ctx_t, name: *mut ngx_str_t) -> ngx_int_t {
    let ctx = unsafe { &mut *ctx };
    let scan = unsafe { &mut *ctx.data.cast::<Scan>() };
    let name = unsafe { &*name };

    scan.scanned += 1;

    let Some(key) = key_from_path(name) else {
        return Status::NGX_OK.into();
    };

    // SAFETY: ngx_walk_tree hands out NUL-terminated paths.
    let path = name.data.cast::<c_char>();

    let Some(block_len) = (unsafe { read_header_block(path, scan.buf) }) else {
        return Status::NGX_OK.into();
    };

    // Field-wise borrows: the block is read-only from here, while the counters
    // and the index handle are written.
    let Scan {
        tag,
        header,
        cache,
        buf,
        index,
        purged,
        indexed,
        ..
    } = scan;
    let block = &buf[..block_len];

    let Some(tags) = find_header(block, header) else {
        return Status::NGX_OK.into();
    };

    if split_tags(tags).any(|t| t == *tag) {
        // On the scan path the file is the record, so removing it is what
        // counts as a purge. Clearing the shared-memory node is best-effort:
        // right after a restart the cache loader may not have added it yet.
        let cleared = unsafe { purge::invalidate(&mut **cache, &key) };
        let unlinked = unsafe { libc::unlink(path) } == 0;

        if cleared || unlinked {
            *purged += 1;
        }

        return Status::NGX_OK.into();
    }

    // The file survives, so record what it was already carrying. A scan reads
    // every file's tags anyway; writing them back is what stops the next purge
    // from having to scan again.
    if let Some((shared, alloc)) = index {
        let mut idx = shared.write();
        let mut wrote = false;

        for t in split_tags(tags) {
            // A full zone is not a scan failure: the answer above is already
            // correct, only the next lookup stays slow.
            if crate::index::insert(&mut idx, alloc, t, &key).is_err() {
                break;
            }
            wrote = true;
        }

        if wrote {
            *indexed += 1;
        }
    }

    Status::NGX_OK.into()
}

/// Recovers the cache key from a cache file's path.
///
/// NGINX names each file after the hex of its key, so the trailing 32
/// characters are the key. Temporary files carry a numeric suffix and are
/// skipped, matching the check NGINX's own cache loader makes.
fn key_from_path(name: &ngx_str_t) -> Option<CacheKey> {
    const HEX_LEN: usize = 2 * CACHE_KEY_LEN;

    let path = name.as_bytes();
    if path.len() < HEX_LEN {
        return None;
    }

    // ".1234567890" appended to an otherwise complete name.
    if path.len() >= HEX_LEN + 11 && path[path.len() - 11] == b'.' {
        return None;
    }

    let mut key: CacheKey = [0; CACHE_KEY_LEN];
    hex_decode(&path[path.len() - HEX_LEN..], &mut key).then_some(key)
}

/// Reads the stored response header block of a cache file into `buf`.
///
/// Returns how many bytes of `buf` the block occupies. The block sits between
/// `header_start` and `body_start`, both recorded in the file's own header.
unsafe fn read_header_block(path: *const c_char, buf: &mut [u8]) -> Option<usize> {
    let fd = unsafe { libc::open(path, libc::O_RDONLY) };
    if fd < 0 {
        return None;
    }

    let block = unsafe { read_header_block_fd(fd, buf) };
    unsafe { libc::close(fd) };
    block
}

unsafe fn read_header_block_fd(fd: i32, buf: &mut [u8]) -> Option<usize> {
    let mut header: ngx_http_file_cache_header_t = unsafe { mem::zeroed() };
    let size = mem::size_of::<ngx_http_file_cache_header_t>();

    let n = unsafe { libc::pread(fd, ptr::from_mut(&mut header).cast::<c_void>(), size, 0) };
    if n != size as isize || header.version as u32 != NGX_HTTP_CACHE_VERSION {
        return None;
    }

    let start = header.header_start as usize;
    let end = header.body_start as usize;
    if end <= start || end - start > buf.len() {
        return None;
    }

    let len = end - start;
    let n = unsafe {
        libc::pread(
            fd,
            buf.as_mut_ptr().cast::<c_void>(),
            len,
            start as libc::off_t,
        )
    };
    if n != len as isize {
        return None;
    }

    Some(len)
}
