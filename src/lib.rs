//! Surrogate-key (tag) based cache purging for NGINX.
//!
//! An upstream response carries a header listing one or more tags, e.g.
//!
//! ```text
//! xkey: product-1234 category-shoes
//! ```
//!
//! Every cacheable response is recorded in a shared-memory index mapping each
//! tag to the set of cache keys carrying it.  A purge request naming a tag then
//! invalidates every cache entry in that set, without walking the cache
//! directory.
//!
//! ```nginx
//! http {
//!     proxy_cache_path /var/cache/nginx keys_zone=CACHE:100m levels=1:2;
//!     xkey_zone xkey:32m;
//!
//!     server {
//!         location / {
//!             proxy_pass  http://backend;
//!             proxy_cache CACHE;
//!         }
//!
//!         location /purge {
//!             xkey_purge CACHE;
//!         }
//!     }
//! }
//! ```
#![no_std]

use core::ffi::{c_char, c_void};
use core::ptr::{self, NonNull};

use nginx_sys::{
    NGX_CONF_FLAG, NGX_CONF_NOARGS, NGX_CONF_TAKE1, NGX_CONF_TAKE2, NGX_HTTP_LOC_CONF,
    NGX_HTTP_LOC_CONF_OFFSET, NGX_HTTP_MAIN_CONF, NGX_HTTP_MAIN_CONF_OFFSET, NGX_HTTP_MODULE,
    NGX_HTTP_SRV_CONF, NGX_LOG_EMERG, ngx_command_t, ngx_conf_set_flag_slot, ngx_conf_t,
    ngx_flag_t, ngx_http_conf_ctx_t, ngx_http_file_cache_t, ngx_http_module_t, ngx_http_request_t,
    ngx_int_t, ngx_module_t, ngx_parse_size, ngx_shared_memory_add, ngx_shm_zone_t, ngx_str_t,
    ngx_uint_t,
};
use ngx::core::{NGX_CONF_ERROR, NGX_CONF_OK, Status};
use ngx::http::{HttpModule, HttpModuleLocationConf, HttpModuleMainConf, Merge, NgxHttpCoreModule};
use ngx::{ngx_conf_log_error, ngx_string};

mod index;
mod purge;
mod record;
mod scan;
mod status;

use index::Shared;

pub use xkey_core::{CACHE_KEY_LEN, CacheKey};

// The layout xkey-core assumes must be the one NGINX actually uses.
const _: () = assert!(CACHE_KEY_LEN == nginx_sys::NGX_HTTP_CACHE_KEY_LEN as usize);

struct HttpXkeyModule;

impl HttpModule for HttpXkeyModule {
    fn module() -> &'static ngx_module_t {
        unsafe { &*ptr::addr_of!(ngx_http_xkey_module) }
    }

    unsafe extern "C" fn postconfiguration(cf: *mut ngx_conf_t) -> ngx_int_t {
        unsafe { record::install_filter(cf) }
    }
}

unsafe impl HttpModuleMainConf for HttpXkeyModule {
    type MainConf = XkeyMainConf;
}

unsafe impl HttpModuleLocationConf for HttpXkeyModule {
    type LocationConf = XkeyLocConf;
}

/// `http`-level configuration: the tag index zone and the response header to read.
#[derive(Debug)]
pub struct XkeyMainConf {
    /// Zone holding the tag index.  Null until `xkey_zone` is seen.
    pub shm_zone: *mut ngx_shm_zone_t,
    /// Response header listing the tags.  Defaults to `xkey`.
    pub header: ngx_str_t,
}

impl Default for XkeyMainConf {
    fn default() -> Self {
        Self {
            shm_zone: ptr::null_mut(),
            header: ngx_string!("xkey"),
        }
    }
}

/// `location`-level configuration: which cache a purge endpoint acts on.
#[derive(Debug)]
#[repr(C)]
pub struct XkeyLocConf {
    /// The `proxy_cache_path` zone named by `xkey_purge`.
    pub cache_zone: *mut ngx_shm_zone_t,
    /// Whether a tag missing from the index falls back to a directory scan.
    pub fallback: ngx_flag_t,
}

impl Default for XkeyLocConf {
    fn default() -> Self {
        Self {
            cache_zone: ptr::null_mut(),
            fallback: NGX_CONF_UNSET,
        }
    }
}

/// NGINX's "not configured" marker for a flag slot.
const NGX_CONF_UNSET: ngx_flag_t = -1;

impl Merge for XkeyLocConf {
    // The endpoint itself is not inherited: it is declared per location and
    // installs a content handler there. Only the fallback setting inherits.
    fn merge(&mut self, prev: &Self) -> Result<(), ngx::http::MergeConfigError> {
        if self.fallback == NGX_CONF_UNSET {
            self.fallback = if prev.fallback == NGX_CONF_UNSET {
                1
            } else {
                prev.fallback
            };
        }
        Ok(())
    }
}

static mut NGX_HTTP_XKEY_COMMANDS: [ngx_command_t; 6] = [
    ngx_command_t {
        name: ngx_string!("xkey_zone"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1 | NGX_CONF_TAKE2) as ngx_uint_t,
        set: Some(ngx_http_xkey_zone),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("xkey_header"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_http_xkey_header),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("xkey_purge"),
        type_: (NGX_HTTP_LOC_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_http_xkey_purge),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("xkey_purge_fallback"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_HTTP_SRV_CONF | NGX_HTTP_LOC_CONF | NGX_CONF_FLAG)
            as ngx_uint_t,
        set: Some(ngx_conf_set_flag_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(XkeyLocConf, fallback),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("xkey_status"),
        type_: (NGX_HTTP_LOC_CONF | NGX_CONF_NOARGS) as ngx_uint_t,
        set: Some(ngx_http_xkey_status),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t::empty(),
];

static NGX_HTTP_XKEY_MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration: None,
    postconfiguration: Some(HttpXkeyModule::postconfiguration),
    create_main_conf: Some(HttpXkeyModule::create_main_conf),
    init_main_conf: None,
    create_srv_conf: None,
    merge_srv_conf: None,
    create_loc_conf: Some(HttpXkeyModule::create_loc_conf),
    merge_loc_conf: Some(HttpXkeyModule::merge_loc_conf),
};

#[cfg(feature = "export-modules")]
ngx::ngx_modules!(ngx_http_xkey_module);

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), unsafe(no_mangle))]
pub static mut ngx_http_xkey_module: ngx_module_t = ngx_module_t {
    ctx: &raw const NGX_HTTP_XKEY_MODULE_CTX as _,
    commands: unsafe { &raw mut NGX_HTTP_XKEY_COMMANDS[0] },
    type_: NGX_HTTP_MODULE as _,
    ..ngx_module_t::default()
};

/// `xkey_zone <name>[:<size>];` or `xkey_zone <name> <size>;`
extern "C" fn ngx_http_xkey_zone(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let cf = unsafe { cf.as_mut().unwrap() };
    let xmcf = unsafe {
        conf.cast::<XkeyMainConf>()
            .as_mut()
            .expect("xkey main config")
    };

    if !xmcf.shm_zone.is_null() {
        return NGX_CONF_ERROR;
    }

    debug_assert!(!cf.args.is_null() && unsafe { (*cf.args).nelts >= 2 });
    let args = unsafe { (*cf.args).as_slice_mut::<ngx_str_t>() };

    // Accept both "name size" and the "name:size" spelling used by
    // proxy_cache_path's keys_zone, so the config reads consistently.
    let (mut name, mut size_arg) = if args.len() >= 3 {
        (args[1], args[2])
    } else {
        let bytes = args[1].as_bytes();
        let Some(colon) = bytes.iter().position(|c| *c == b':') else {
            ngx_conf_log_error!(NGX_LOG_EMERG, cf, "\"xkey_zone\" requires a zone size");
            return NGX_CONF_ERROR;
        };
        let (n, s) = bytes.split_at(colon);
        (
            ngx_str_t {
                data: n.as_ptr().cast_mut(),
                len: n.len(),
            },
            ngx_str_t {
                data: s[1..].as_ptr().cast_mut(),
                len: s.len() - 1,
            },
        )
    };

    let size = unsafe { ngx_parse_size(&raw mut size_arg) };
    if size == -1 {
        ngx_conf_log_error!(NGX_LOG_EMERG, cf, "invalid \"xkey_zone\" size");
        return NGX_CONF_ERROR;
    }

    xmcf.shm_zone = unsafe {
        ngx_shared_memory_add(
            cf,
            &raw mut name,
            size as usize,
            (&raw mut ngx_http_xkey_module).cast(),
        )
    };

    let Some(shm_zone) = (unsafe { xmcf.shm_zone.as_mut() }) else {
        return NGX_CONF_ERROR;
    };

    shm_zone.init = Some(ngx_http_xkey_zone_init);
    shm_zone.data = ptr::from_mut(xmcf).cast();

    NGX_CONF_OK
}

extern "C" fn ngx_http_xkey_zone_init(
    shm_zone: *mut ngx_shm_zone_t,
    _data: *mut c_void,
) -> ngx_int_t {
    // On reload NGINX reuses the mapping and hands back the previous `data`,
    // so an index built by the old cycle survives untouched.  `shared()` only
    // allocates when the slab pool is fresh.
    match index::shared(unsafe { &mut *shm_zone }) {
        Ok(_) => Status::NGX_OK.into(),
        Err(e) => e.into(),
    }
}

/// `xkey_header <name>;`
extern "C" fn ngx_http_xkey_header(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let cf = unsafe { cf.as_mut().unwrap() };
    let xmcf = unsafe {
        conf.cast::<XkeyMainConf>()
            .as_mut()
            .expect("xkey main config")
    };

    debug_assert!(!cf.args.is_null() && unsafe { (*cf.args).nelts >= 2 });
    let args = unsafe { (*cf.args).as_slice_mut() };

    xmcf.header = args[1];

    NGX_CONF_OK
}

/// `xkey_purge <cache_zone>;`
extern "C" fn ngx_http_xkey_purge(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let cf = unsafe { cf.as_mut().unwrap() };
    let xlcf = unsafe {
        conf.cast::<XkeyLocConf>()
            .as_mut()
            .expect("xkey loc config")
    };

    if !xlcf.cache_zone.is_null() {
        return NGX_CONF_ERROR;
    }

    debug_assert!(!cf.args.is_null() && unsafe { (*cf.args).nelts >= 2 });
    let args = unsafe { (*cf.args).as_slice_mut() };
    let mut name = args[1];

    // The zone must be looked up with the tag NGINX itself used when the
    // proxy module declared it, or ngx_shared_memory_add() rejects it as
    // "already declared for a different use".
    xlcf.cache_zone = unsafe {
        ngx_shared_memory_add(
            cf,
            &raw mut name,
            0,
            (&raw mut ngx_http_proxy_module).cast(),
        )
    };
    if xlcf.cache_zone.is_null() {
        return NGX_CONF_ERROR;
    }

    let ctx = unsafe { cf.ctx.cast::<ngx_http_conf_ctx_t>().as_ref() }.expect("http conf ctx");
    let clcf = NgxHttpCoreModule::location_conf_mut(ctx).expect("core loc conf");
    clcf.handler = Some(purge::handler);

    NGX_CONF_OK
}

/// `xkey_status;`
extern "C" fn ngx_http_xkey_status(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    _conf: *mut c_void,
) -> *mut c_char {
    let cf = unsafe { cf.as_mut().unwrap() };
    let ctx = unsafe { cf.ctx.cast::<ngx_http_conf_ctx_t>().as_ref() }.expect("http conf ctx");
    let clcf = NgxHttpCoreModule::location_conf_mut(ctx).expect("core loc conf");

    clcf.handler = Some(status::handler);

    NGX_CONF_OK
}

unsafe extern "C" {
    /// Declared in `ngx_http_proxy_module.c`; not exposed by any public header
    /// before nginx 1.30.  Only the symbol is needed, as the shared zone tag.
    static mut ngx_http_proxy_module: ngx_module_t;
}

/// Resolves the file cache behind a `proxy_cache_path` zone.
pub fn file_cache(shm_zone: *mut ngx_shm_zone_t) -> Option<NonNull<ngx_http_file_cache_t>> {
    let zone = unsafe { shm_zone.as_ref() }?;
    NonNull::new(zone.data.cast::<ngx_http_file_cache_t>())
}

/// Resolves the tag index for a request, if `xkey_zone` was configured.
pub fn index_for(r: &ngx_http_request_t) -> Option<&'static Shared> {
    let xmcf = HttpXkeyModule::main_conf_mut(r)?;
    let zone = unsafe { xmcf.shm_zone.as_mut() }?;
    index::shared(zone).ok()
}
