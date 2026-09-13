//! The shared-memory tag index.
//!
//! Maps a tag to the set of cache keys carrying it.  Lives in its own zone
//! rather than borrowing the cache zone's slab, so that `keys_zone` sizing
//! stays meaningful and an exhausted tag index cannot starve the cache itself.

use nginx_sys::ngx_shm_zone_t;
use ngx::allocator::AllocError;
use ngx::collections::{RbTreeMap, Vec};
use ngx::core::{NgxString, SlabPool, Status};
use ngx::sync::RwLock;

use crate::CacheKey;

/// Cache keys recorded against one tag.
pub type Keys = Vec<CacheKey, SlabPool>;

/// Tag to cache keys.
pub type TagIndex = RbTreeMap<NgxString<SlabPool>, Keys, SlabPool>;

/// The index as stored in shared memory, guarded for cross-worker access.
pub type Shared = RwLock<TagIndex>;

/// Returns the index held in `shm_zone`, allocating it on first use.
///
/// Across a reload NGINX reuses the mapping when the zone name, tag and size
/// are unchanged, so `data` is already populated and the existing index is
/// adopted as it stands.
pub fn shared(shm_zone: &mut ngx_shm_zone_t) -> Result<&'static Shared, Status> {
    let mut alloc = unsafe { SlabPool::from_shm_zone(shm_zone) }.ok_or(Status::NGX_ERROR)?;

    if alloc.as_mut().data.is_null() {
        let index = TagIndex::try_new_in(alloc.clone()).map_err(|_| Status::NGX_ERROR)?;
        let shared = RwLock::new(index);

        alloc.as_mut().data = ngx::allocator::allocate(shared, &alloc)
            .map_err(|_| Status::NGX_ERROR)?
            .as_ptr()
            .cast();
    }

    // SAFETY: the allocation above lives in shared memory for as long as the
    // zone does, which outlives every request that can reach it.
    unsafe {
        alloc
            .as_ref()
            .data
            .cast::<Shared>()
            .as_ref()
            .ok_or(Status::NGX_ERROR)
    }
}

/// Records `key` against `tag`, ignoring a key already present.
pub fn insert(
    index: &mut TagIndex,
    alloc: &SlabPool,
    tag: &[u8],
    key: &CacheKey,
) -> Result<(), AllocError> {
    if let Some(keys) = index.get_mut(tag) {
        if !keys.contains(key) {
            keys.try_reserve(1).map_err(|_| AllocError)?;
            keys.push(*key);
        }
        return Ok(());
    }

    let name = NgxString::try_from_bytes_in(tag, alloc.clone()).map_err(|_| AllocError)?;
    let mut keys = Keys::new_in(alloc.clone());
    keys.try_reserve(1).map_err(|_| AllocError)?;
    keys.push(*key);

    index.try_insert(name, keys)?;

    Ok(())
}

/// Removes `tag` and returns the cache keys that were recorded against it.
pub fn take(index: &mut TagIndex, tag: &[u8]) -> Option<Keys> {
    index.remove(tag)
}
