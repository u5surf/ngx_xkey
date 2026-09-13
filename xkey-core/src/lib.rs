//! Pure logic behind `ngx_xkey`.
//!
//! Nothing here references an NGINX symbol, so it links and runs in an
//! ordinary test binary.  The parts worth isolating are the ones where a
//! mistake is silent rather than loud: how a tag header is split, and how a
//! cache key maps onto the layout NGINX uses for its cache rbtree.

#![cfg_attr(not(test), no_std)]

/// Length of an NGINX cache key: the MD5 of the cache key string.
pub const CACHE_KEY_LEN: usize = 16;

/// A cache entry is addressed by the MD5 of its cache key.
pub type CacheKey = [u8; CACHE_KEY_LEN];

/// Splits a tag header value into individual tags.
///
/// Tags are separated by spaces, tabs or commas, following `xkey`'s
/// whitespace-separated form while tolerating the comma-separated spelling
/// that surrogate-key headers often use.  Empty runs are dropped, so repeated
/// or trailing separators are harmless.
pub fn split_tags(value: &[u8]) -> impl Iterator<Item = &[u8]> {
    value
        .split(|c| matches!(c, b' ' | b'\t' | b',' | b'\r' | b'\n'))
        .filter(|tag| !tag.is_empty())
}

/// The key NGINX stores in the cache rbtree node.
///
/// NGINX copies the leading `size_of::<usize>()` bytes of the cache key into
/// an integer field verbatim, so the value is native-endian and its width
/// follows the pointer width.  Reconstructing it any other way yields a key
/// that never matches, which fails silently: the lookup simply finds nothing.
pub fn node_key(key: &CacheKey) -> usize {
    let prefix = core::mem::size_of::<usize>();
    let mut bytes = [0u8; core::mem::size_of::<usize>()];
    bytes.copy_from_slice(&key[..prefix]);
    usize::from_ne_bytes(bytes)
}

/// The remainder of the cache key, held in the node's own `key` field and
/// compared bytewise when the integer keys are equal.
pub fn node_key_rest(key: &CacheKey) -> &[u8] {
    &key[core::mem::size_of::<usize>()..]
}

/// Writes `src` to `dst` as lowercase hex, as NGINX names its cache files.
///
/// Returns the number of bytes written, or `None` if `dst` is too small.
pub fn hex_encode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let needed = src.len() * 2;
    if dst.len() < needed {
        return None;
    }

    for (i, byte) in src.iter().enumerate() {
        dst[i * 2] = HEX[(byte >> 4) as usize];
        dst[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }

    Some(needed)
}

/// Formats `n` into `buf`, returning the populated slice.
///
/// `buf` must hold 20 bytes, enough for any `usize` on a 64-bit target.
pub fn format_usize(buf: &mut [u8; 20], mut n: usize) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(value: &[u8]) -> Vec<&[u8]> {
        split_tags(value).collect()
    }

    #[test]
    fn splits_on_spaces() {
        assert_eq!(
            tags(b"product-1234 category-shoes"),
            vec![&b"product-1234"[..], &b"category-shoes"[..]]
        );
    }

    #[test]
    fn splits_on_commas_and_tabs() {
        assert_eq!(tags(b"a,b\tc"), vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }

    #[test]
    fn drops_empty_runs() {
        assert_eq!(tags(b"  a ,,  b  "), vec![&b"a"[..], &b"b"[..]]);
    }

    #[test]
    fn drops_trailing_newline() {
        assert_eq!(tags(b"a b\r\n"), vec![&b"a"[..], &b"b"[..]]);
    }

    #[test]
    fn empty_header_yields_no_tags() {
        assert!(tags(b"").is_empty());
        assert!(tags(b"   ").is_empty());
    }

    #[test]
    fn single_tag_survives_intact() {
        assert_eq!(tags(b"product-1234"), vec![&b"product-1234"[..]]);
    }

    /// The node key is a verbatim copy of the leading bytes, not a
    /// byte-order conversion.  Pinning this down matters: the widely used C
    /// purge module reconstructs it as a 4-byte big-endian value, which on a
    /// 64-bit little-endian host matches nothing.
    #[test]
    fn node_key_copies_bytes_verbatim() {
        let mut key: CacheKey = [0; CACHE_KEY_LEN];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }

        let prefix = core::mem::size_of::<usize>();
        let expected = {
            let mut bytes = [0u8; core::mem::size_of::<usize>()];
            bytes.copy_from_slice(&key[..prefix]);
            usize::from_ne_bytes(bytes)
        };

        assert_eq!(node_key(&key), expected);
        assert_ne!(node_key(&key), u32::from_be_bytes([0, 1, 2, 3]) as usize);
    }

    #[test]
    fn node_key_and_rest_cover_the_whole_key() {
        let key: CacheKey = [0xab; CACHE_KEY_LEN];
        assert_eq!(
            node_key_rest(&key).len(),
            CACHE_KEY_LEN - size_of::<usize>()
        );
    }

    #[test]
    fn distinct_keys_give_distinct_nodes() {
        let mut a: CacheKey = [0; CACHE_KEY_LEN];
        let mut b: CacheKey = [0; CACHE_KEY_LEN];
        a[0] = 1;
        b[0] = 2;
        assert_ne!(node_key(&a), node_key(&b));
    }

    /// Keys differing only past the prefix share a node key and must be told
    /// apart by the remainder, which is what the rbtree tie-break compares.
    #[test]
    fn keys_differing_late_share_a_node_key() {
        let mut a: CacheKey = [0; CACHE_KEY_LEN];
        let mut b: CacheKey = [0; CACHE_KEY_LEN];
        a[CACHE_KEY_LEN - 1] = 1;
        b[CACHE_KEY_LEN - 1] = 2;

        assert_eq!(node_key(&a), node_key(&b));
        assert_ne!(node_key_rest(&a), node_key_rest(&b));
    }

    #[test]
    fn hex_encodes_lowercase() {
        let mut out = [0u8; 32];
        let key: CacheKey = [
            0xa7, 0x70, 0xde, 0xe4, 0x80, 0x52, 0x5b, 0xf3, 0x4f, 0x9a, 0x7b, 0xa0, 0x8e, 0x55,
            0x2c, 0x3d,
        ];

        assert_eq!(hex_encode(&key, &mut out), Some(32));
        assert_eq!(&out[..], b"a770dee480525bf34f9a7ba08e552c3d");
    }

    #[test]
    fn hex_refuses_a_short_buffer() {
        let mut out = [0u8; 31];
        assert_eq!(hex_encode(&[0u8; 16], &mut out), None);
    }

    #[test]
    fn formats_counts() {
        let mut buf = [0u8; 20];
        assert_eq!(format_usize(&mut buf, 0), b"0");
        assert_eq!(format_usize(&mut buf, 7), b"7");
        assert_eq!(format_usize(&mut buf, 10), b"10");
        assert_eq!(format_usize(&mut buf, 1234), b"1234");
        assert_eq!(format_usize(&mut buf, usize::MAX).len(), 20);
    }
}
