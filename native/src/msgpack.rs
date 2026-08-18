//! Overseer — msgpack (rmpv) navigation helpers, shared by the response-hook consumers
//! (the response hook's player-id parse). Generic tree walking
//! only; no feature logic lives here.

#![allow(dead_code)]

use rmpv::Value;

/// Decode a full msgpack tree from raw bytes, or None on malformed input. The one shared decode —
/// callers decode once and fan the `Value` out by reference to every consumer.
pub fn decode(bytes: &[u8]) -> Option<Value> {
    let mut cur = std::io::Cursor::new(bytes);
    rmpv::decode::read_value(&mut cur).ok()
}

/// Value stored under `key` in a msgpack map, or None.
pub fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(m) = v {
        m.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, val)| val)
    } else {
        None
    }
}

/// The array backing a `Value::Array`, or None.
pub fn as_arr(v: &Value) -> Option<&Vec<Value>> {
    if let Value::Array(a) = v {
        Some(a)
    } else {
        None
    }
}

/// Collect every value stored under `key` anywhere in the tree (depth-first).
pub fn find_key<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Map(m) => {
            for (k, val) in m {
                if k.as_str() == Some(key) {
                    out.push(val);
                }
                find_key(val, key, out);
            }
        }
        Value::Array(a) => {
            for val in a {
                find_key(val, key, out);
            }
        }
        _ => {}
    }
}

/// True if `needle` occurs as a contiguous byte subslice of `hay`.
///
/// First-byte skip: `windows().any()` compares the full needle at every offset, which is a lot of
/// work on a multi-megabyte response. Testing the first byte before the full comparison cuts the
/// common case to one byte-compare per offset.
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return needle.is_empty();
    }
    let first = needle[0];
    let last = hay.len() - needle.len();
    let mut i = 0usize;
    while i <= last {
        match hay[i..=last].iter().position(|&b| b == first) {
            Some(off) => {
                let at = i + off;
                if &hay[at..at + needle.len()] == needle {
                    return true;
                }
                i = at + 1;
            }
            None => return false,
        }
    }
    false
}

/// Test SEVERAL needles in ONE pass over `hay`, returning a hit flag per needle.
///
/// PERF: the response hook asks six independent `contains()` questions of every decrypted API
/// response, on the game's NETWORK thread. Each call re-scanned the whole payload, so a 5 MB career
/// response was scanned 30 MB worth — per response, forever. This walks the buffer once and checks
/// every needle at each candidate offset, which is the same answer for a sixth of the memory
/// traffic. Needles are short and few, so the inner loop stays trivially cheap.
pub fn contains_any(hay: &[u8], needles: &[&[u8]]) -> Vec<bool> {
    let mut found = vec![false; needles.len()];
    let mut remaining = 0usize;
    // Anything empty or longer than the payload is decided up front and never scanned for.
    let mut live: Vec<usize> = Vec::with_capacity(needles.len());
    for (i, n) in needles.iter().enumerate() {
        if n.is_empty() {
            found[i] = true;
        } else if n.len() <= hay.len() {
            live.push(i);
            remaining += 1;
        }
    }
    if remaining == 0 || hay.is_empty() {
        return found;
    }
    // First-byte table: one 256-entry lookup per input byte decides whether ANY live needle could
    // start here. Without it the inner loop would compare every needle's first byte at every offset
    // (needles × payload); with it, the overwhelmingly common "no needle starts here" case costs a
    // single indexed bool, and the payload is still walked exactly once.
    let mut starts = [false; 256];
    for &i in &live {
        starts[needles[i][0] as usize] = true;
    }
    let last = hay.len();
    for at in 0..last {
        if !starts[hay[at] as usize] {
            continue;
        }
        let mut k = 0;
        while k < live.len() {
            let i = live[k];
            let n = needles[i];
            if n[0] == hay[at] && at + n.len() <= last && &hay[at..at + n.len()] == n {
                found[i] = true;
                live.swap_remove(k); // retire it — later offsets needn't re-test a found needle
                remaining -= 1;
                if remaining == 0 {
                    return found;
                }
                // The first-byte table may now over-accept; rebuilding it is cheap and keeps the
                // fast path tight for the rest of the payload.
                starts = [false; 256];
                for &j in &live {
                    starts[needles[j][0] as usize] = true;
                }
                continue; // `k` now indexes the swapped-in entry
            }
            k += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{contains, contains_any};

    #[test]
    fn contains_matches_the_naive_definition() {
        assert!(contains(b"hello world", b"world"));
        assert!(contains(b"hello world", b"hello"));
        assert!(contains(b"hello world", b"o w"));
        assert!(!contains(b"hello world", b"worlds"));
        assert!(!contains(b"short", b"much longer needle"));
        assert!(contains(b"", b""));
        assert!(!contains(b"", b"x"));
        // A false start on the first byte must not stop the search.
        assert!(contains(b"aab", b"ab"));
        assert!(contains(b"aaaaab", b"aab"));
    }

    #[test]
    fn contains_any_agrees_with_contains() {
        let hay = b"{\"chara_info\":1,\"home_info\":2,\"race_scenario\":\"x\"}";
        let needles: [&[u8]; 5] = [
            b"chara_info",
            b"home_info",
            b"race_scenario",
            b"single_mode_finish_common",
            b"race_horse_data",
        ];
        let got = contains_any(hay, &needles);
        for (i, n) in needles.iter().enumerate() {
            assert_eq!(got[i], contains(hay, n), "needle {i} disagreed");
        }
        assert_eq!(got, vec![true, true, true, false, false]);
    }

    #[test]
    fn contains_any_handles_edges() {
        assert_eq!(contains_any(b"", &[b"a"]), vec![false]);
        assert_eq!(contains_any(b"abc", &[]), Vec::<bool>::new());
        // Overlapping needles all resolve independently.
        assert_eq!(contains_any(b"abcabc", &[b"abc", b"bca", b"cab", b"zzz"]), vec![true, true, true, false]);
        // A needle longer than the payload can't match but must not panic.
        assert_eq!(contains_any(b"ab", &[b"abcdef"]), vec![false]);
    }
}
