//! Port of `util/VarInt.java`: encode and decode Protocol Buffer-style VarInts.
//!
//! `get_var_long` and `put_var_long` are adapted from
//! [Bazel](https://github.com/bazelbuild/bazel/blob/master/src/main/java/com/google/devtools/build/lib/util/VarInt.java).

/// Reads an up to 64 bit varint from the front of `src`, advancing it to the first byte after the decoded varint, or
/// returns `None` if `src` ends before the varint is complete.
#[inline]
pub fn try_get_var_long(src: &mut &[u8]) -> Option<i64> {
    let bytes = *src;
    // fast path: single-byte values are the most common
    let &first = bytes.first()?;
    if first & 0x80 == 0 {
        *src = &bytes[1..];
        return Some(first as i64);
    }
    let mut result = (first & 0x7f) as u64;
    let mut i = 1;
    while i < 10 {
        let &b = bytes.get(i)?;
        i += 1;
        if i == 10 {
            // the 10th byte only contributes the top bit
            result |= (b as u64) << 63;
            break;
        }
        result |= ((b & 0x7f) as u64) << (7 * (i - 1));
        if b & 0x80 == 0 {
            break;
        }
    }
    *src = &bytes[i..];
    Some(result as i64)
}

#[inline]
pub fn get_var_long(src: &mut &[u8]) -> i64 {
    try_get_var_long(src).expect("buffer underflow reading varint")
}

/// Encodes a long integer in a variable-length encoding, 7 bits per byte, appending to `out`.
#[inline]
pub fn put_var_long(v: i64, out: &mut Vec<u8>) {
    let mut v = v as u64;
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let mut dir = Vec::new();
        put_var_long(0, &mut dir);
        put_var_long(1, &mut dir);
        put_var_long(i64::MAX, &mut dir);
        put_var_long(i64::MIN, &mut dir);
        let mut output = dir.as_slice();
        assert_eq!(0, get_var_long(&mut output));
        assert_eq!(1, get_var_long(&mut output));
        assert_eq!(i64::MAX, get_var_long(&mut output));
        assert_eq!(i64::MIN, get_var_long(&mut output));
        assert!(output.is_empty());
        assert_eq!(None, try_get_var_long(&mut output));
    }

    #[test]
    fn test_unsigned_encoding() {
        let rawbytes: &[u8] = &[0, 1, 127, 0xe5, 0x8e, 0x26];
        let mut buf = rawbytes;
        assert_eq!(0, get_var_long(&mut buf));
        assert_eq!(1, get_var_long(&mut buf));
        assert_eq!(127, get_var_long(&mut buf));
        assert_eq!(624485, get_var_long(&mut buf));

        let max_safe_js_integer: &[u8] = &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf];
        assert_eq!(9007199254740991, get_var_long(&mut &max_safe_js_integer[..]));
    }
}
