//! The shared-memory tag index.
//!
//! Maps a tag to the set of cache keys carrying it. Lives in its own zone
//! rather than borrowing the cache zone's slab, so that `keys_zone` sizing
//! stays meaningful and an exhausted tag index cannot starve the cache itself.
//!
//! Nothing removes a key except purging its tag, so entries for cache files
//! NGINX has since evicted would otherwise pile up forever: NGINX offers no
//! notification when it drops an entry. The index therefore keeps its own LRU
//! over tags and evicts the coldest to make room, which is safe only because
//! the scan in [`crate::scan`] is authoritative. Losing an entry costs a scan,
//! never a wrong answer.

use core::{mem, ptr};

use nginx_sys::{
    ngx_queue_init, ngx_queue_insert_after, ngx_queue_remove, ngx_queue_t, ngx_shm_zone_t,
};
use ngx::allocator::AllocError;
use ngx::collections::{RbTreeMap, Vec};
use ngx::core::{NgxString, SlabPool, Status};
use ngx::sync::RwLock;
use xkey_core::{CacheKey, MAX_TAG_LEN};

/// Most cache keys recorded against a single tag.
///
/// A tag is meant to name a group of entries. One that names hundreds of
/// thousands is really a per-URL label duplicating the cache key, and would
/// crowd out every other tag. Beyond this the oldest key is dropped and only
/// a scan will find it.
pub const MAX_KEYS_PER_TAG: usize = 65536;

/// Evictions attempted before an insert gives up.
const EVICT_ATTEMPTS: usize = 64;

/// Cache keys recorded against one tag.
pub type Keys = Vec<CacheKey, SlabPool>;

/// The index as stored in shared memory, guarded for cross-worker access.
pub type Shared = RwLock<TagIndex>;

/// One tag's entry.
///
/// `lru` is first so that a queue link can be cast straight back to the entry.
/// Its address has to stay put, which it does: the map allocates each entry
/// once and never moves it while it is present.
#[repr(C)]
pub struct Entry {
    lru: ngx_queue_t,
    /// A second copy of the tag. An eviction starts from the LRU link and has
    /// to name the map key to remove it.
    tag: NgxString<SlabPool>,
    keys: Keys,
}

/// Tag to cache keys, with an LRU over the tags.
pub struct TagIndex {
    map: RbTreeMap<NgxString<SlabPool>, Entry, SlabPool>,
    /// Most recently used first. Self-referential, so it can only be
    /// initialised once the index sits at its final address.
    lru: ngx_queue_t,
    /// Tags currently held.
    pub tags: usize,
    /// Cache keys currently held, summed over every tag.
    pub keys: usize,
    /// Tags dropped to make room.
    pub evicted: u64,
    /// Keys dropped because a tag reached [`MAX_KEYS_PER_TAG`].
    pub dropped: u64,
}

impl TagIndex {
    /// Records `key` against `tag`, ignoring a key already present.
    pub fn insert(
        &mut self,
        alloc: &SlabPool,
        tag: &[u8],
        key: &CacheKey,
    ) -> Result<(), AllocError> {
        // An eviction copies the tag onto the stack to name it, so a tag that
        // would not fit there must never enter the index.
        if tag.is_empty() || tag.len() > MAX_TAG_LEN {
            return Err(AllocError);
        }

        // Using a tag counts as touching it, and it also puts it out of reach
        // of the evictions below: growing an entry must not be able to drop
        // the very entry it is growing.
        if let Some(entry) = self.map.get_mut(tag) {
            let link = &raw mut entry.lru;
            unsafe { self.touch(link) };
        }

        for _ in 0..EVICT_ATTEMPTS {
            let result = if self.map.get(tag).is_some() {
                self.append(tag, key)
            } else {
                self.create(alloc, tag, key)
            };

            match result {
                Ok(()) => return Ok(()),
                // Appending needs room as much as creating does: a tag's key
                // list grows by reallocating out of the same slab.
                Err(e) => {
                    if !self.evict() {
                        return Err(e);
                    }
                }
            }
        }

        Err(AllocError)
    }

    /// Adds a key to a tag that is already present.
    fn append(&mut self, tag: &[u8], key: &CacheKey) -> Result<(), AllocError> {
        let Some(entry) = self.map.get_mut(tag) else {
            return Err(AllocError);
        };

        let mut dropped = 0;

        let mut added = 0;

        if !entry.keys.contains(key) {
            if entry.keys.len() >= MAX_KEYS_PER_TAG {
                entry.keys.remove(0);
                dropped = 1;
            }
            entry.keys.try_reserve(1).map_err(|_| AllocError)?;
            entry.keys.push(*key);
            added = 1;
        }

        self.dropped += dropped;
        self.keys += added - dropped as usize;

        Ok(())
    }

    /// Adds a tag that is not yet present.
    fn create(&mut self, alloc: &SlabPool, tag: &[u8], key: &CacheKey) -> Result<(), AllocError> {
        let name = NgxString::try_from_bytes_in(tag, alloc.clone()).map_err(|_| AllocError)?;
        let dup = NgxString::try_from_bytes_in(tag, alloc.clone()).map_err(|_| AllocError)?;

        let mut keys = Keys::new_in(alloc.clone());
        keys.try_reserve(1).map_err(|_| AllocError)?;
        keys.push(*key);

        // Zeroed rather than initialised: the link is only valid once the map
        // has placed the entry at its final address.
        let entry = Entry {
            lru: unsafe { mem::zeroed() },
            tag: dup,
            keys,
        };
        let slot = self.map.try_insert(name, entry)?;
        let link = &raw mut slot.lru;

        unsafe {
            ngx_queue_insert_after(&raw mut self.lru, link);
        }

        self.tags += 1;
        self.keys += 1;

        Ok(())
    }

    /// Moves an entry to the front of the LRU.
    ///
    /// # Safety
    ///
    /// `link` must be the queue link of an entry currently in this index.
    unsafe fn touch(&mut self, link: *mut ngx_queue_t) {
        unsafe {
            ngx_queue_remove(link);
            ngx_queue_insert_after(&raw mut self.lru, link);
        }
    }

    /// Drops the coldest tag. Returns whether anything was there to drop.
    fn evict(&mut self) -> bool {
        let head = &raw mut self.lru;
        let last = unsafe { (*head).prev };

        if last.is_null() || ptr::eq(last, head) {
            return false;
        }

        // The entry owns the tag it is keyed by, and removing it frees that
        // memory, so name it from a copy.
        let entry = last.cast::<Entry>();
        let mut name = [0u8; MAX_TAG_LEN];
        let len = {
            let tag: &[u8] = unsafe { (*entry).tag.as_ref() };
            let len = tag.len().min(MAX_TAG_LEN);
            name[..len].copy_from_slice(&tag[..len]);
            len
        };

        // Unlink before the map drops the entry and with it the link.
        unsafe { ngx_queue_remove(last) };

        if let Some(entry) = self.map.remove(&name[..len]) {
            self.tags -= 1;
            self.keys -= entry.keys.len();
            self.evicted += 1;
            return true;
        }

        false
    }

    /// Removes `tag` and returns the cache keys recorded against it.
    pub fn take(&mut self, tag: &[u8]) -> Option<Keys> {
        let entry = self.map.get_mut(tag)?;
        unsafe { ngx_queue_remove(&raw mut entry.lru) };

        let keys = self.map.remove(tag).map(|entry| entry.keys)?;

        self.tags -= 1;
        self.keys -= keys.len();

        Some(keys)
    }
}

/// Returns the index held in `shm_zone`, allocating it on first use.
///
/// Across a reload NGINX reuses the mapping when the zone name, tag and size
/// are unchanged, so `data` is already populated and the existing index is
/// adopted as it stands.
pub fn shared(shm_zone: &mut ngx_shm_zone_t) -> Result<&'static Shared, Status> {
    let mut alloc = unsafe { SlabPool::from_shm_zone(shm_zone) }.ok_or(Status::NGX_ERROR)?;

    if alloc.as_mut().data.is_null() {
        let map = RbTreeMap::try_new_in(alloc.clone()).map_err(|_| Status::NGX_ERROR)?;
        let index = TagIndex {
            map,
            lru: unsafe { mem::zeroed() },
            tags: 0,
            keys: 0,
            evicted: 0,
            dropped: 0,
        };

        let shared =
            ngx::allocator::allocate(RwLock::new(index), &alloc).map_err(|_| Status::NGX_ERROR)?;

        // The queue head points at itself, so it is only meaningful once the
        // index has reached the address it will keep.
        unsafe {
            let mut index = (*shared.as_ptr()).write();
            ngx_queue_init(&raw mut index.lru);
        }

        alloc.as_mut().data = shared.as_ptr().cast();

        // Running out of room is how eviction is triggered, not a fault, so
        // the slab must not log every attempt as critical.
        alloc.as_mut().set_log_nomem(0);
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
