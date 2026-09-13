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

/// Longest tag accepted on a purge request.
///
/// Matches the per-key limit Fastly documents for `Surrogate-Key`, which is a
/// reasonable ceiling for an opaque label and keeps the tag copyable onto the
/// stack while a purge runs.
pub const MAX_TAG_LEN: usize = 1024;

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

/// Decodes `src` as lowercase or uppercase hex into `dst`.
///
/// Returns `false` and leaves `dst` partly written if `src` is not hex or the
/// lengths do not correspond.  NGINX names each cache file after the hex of
/// its key, so this is how a path maps back to an entry.
pub fn hex_decode(src: &[u8], dst: &mut [u8]) -> bool {
    if src.len() != dst.len() * 2 {
        return false;
    }

    for (i, out) in dst.iter_mut().enumerate() {
        let (hi, lo) = (nibble(src[i * 2]), nibble(src[i * 2 + 1]));
        match (hi, lo) {
            (Some(hi), Some(lo)) => *out = (hi << 4) | lo,
            _ => return false,
        }
    }

    true
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Finds a header's value in a raw HTTP response header block.
///
/// The block is what NGINX stores in a cache file between `header_start` and
/// `body_start`: a status line followed by headers, terminated by CRLF or LF.
/// The match on the name is case-insensitive and the value is trimmed.
pub fn find_header<'a>(block: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for line in block.split(|c| *c == b'\n') {
        let line = trim(line);
        // The status line has no colon; skip it and anything else malformed.
        let Some(colon) = line.iter().position(|c| *c == b':') else {
            continue;
        };
        let (found, value) = line.split_at(colon);

        if found.eq_ignore_ascii_case(name) {
            return Some(trim(&value[1..]));
        }
    }

    None
}

/// Whether a stored header block carries `tag` under the header `name`.
pub fn block_has_tag(block: &[u8], name: &[u8], tag: &[u8]) -> bool {
    match find_header(block, name) {
        Some(value) => split_tags(value).any(|t| t == tag),
        None => false,
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &[u8] =
        b"HTTP/1.1 200 OK\r\nServer: origin\r\nXKey: product-1234 all\r\nContent-Length: 4\r\n\r\n";

    #[test]
    fn decodes_a_cache_file_name() {
        let mut key = [0u8; CACHE_KEY_LEN];
        assert!(hex_decode(b"a770dee480525bf34f9a7ba08e552c3d", &mut key));
        assert_eq!(key[0], 0xa7);
        assert_eq!(key[CACHE_KEY_LEN - 1], 0x3d);
    }

    #[test]
    fn hex_round_trips() {
        let key: CacheKey = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut encoded = [0u8; 32];
        hex_encode(&key, &mut encoded).unwrap();

        let mut decoded = [0u8; CACHE_KEY_LEN];
        assert!(hex_decode(&encoded, &mut decoded));
        assert_eq!(decoded, key);
    }

    #[test]
    fn hex_decode_accepts_uppercase() {
        let mut key = [0u8; 2];
        assert!(hex_decode(b"AbCd", &mut key));
        assert_eq!(key, [0xab, 0xcd]);
    }

    #[test]
    fn hex_decode_rejects_non_hex_and_bad_lengths() {
        let mut key = [0u8; CACHE_KEY_LEN];
        assert!(!hex_decode(b"a770dee480525bf34f9a7ba08e552c3g", &mut key));
        assert!(!hex_decode(b"abcd", &mut key));
        assert!(!hex_decode(b"", &mut key));
    }

    #[test]
    fn finds_a_header_regardless_of_case() {
        assert_eq!(find_header(BLOCK, b"xkey"), Some(&b"product-1234 all"[..]));
        assert_eq!(find_header(BLOCK, b"SERVER"), Some(&b"origin"[..]));
    }

    #[test]
    fn missing_header_is_none() {
        assert_eq!(find_header(BLOCK, b"surrogate-key"), None);
    }

    #[test]
    fn header_search_tolerates_lf_only_blocks() {
        let block = b"HTTP/1.1 200 OK\nXKey: a b\n\n";
        assert_eq!(find_header(block, b"xkey"), Some(&b"a b"[..]));
    }

    #[test]
    fn block_tag_match_is_exact_per_tag() {
        assert!(block_has_tag(BLOCK, b"xkey", b"product-1234"));
        assert!(block_has_tag(BLOCK, b"xkey", b"all"));
        // A prefix of a tag must not match the tag.
        assert!(!block_has_tag(BLOCK, b"xkey", b"product"));
        // Nor must the whole header value read as one tag.
        assert!(!block_has_tag(BLOCK, b"xkey", b"product-1234 all"));
    }

    #[test]
    fn block_without_the_header_matches_nothing() {
        let block = b"HTTP/1.1 200 OK\r\nServer: origin\r\n\r\n";
        assert!(!block_has_tag(block, b"xkey", b"all"));
    }

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
