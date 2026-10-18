//! Port of `util/VarInt.java`: encode and decode Protocol Buffer-style VarInts.
//!
//! `get_var_long` and `put_var_long` are adapted from
//! [Bazel](https://github.com/bazelbuild/bazel/blob/master/src/main/java/com/google/devtools/build/lib/util/VarInt.java).

/// Reads an up to 64 bit varint from the front of `src`, advancing it to the first byte after the decoded varint, or
/// returns `None` if `src` ends before the varint is complete.
pub fn try_get_var_long(src: &mut &[u8]) -> Option<i64> {
    let mut result: i64 = 0;
    for i in 0..10 {
        let (&b, rest) = src.split_first()?;
        *src = rest;
        if i == 9 {
            result |= (b as i64) << 63;
            break;
        }
        result |= ((b & 0x7f) as i64) << (7 * i);
        if b & 0x80 == 0 {
            break;
        }
    }
    Some(result)
}

/// Reads an up to 64 bit varint from the front of `src`, advancing it to the first byte after the decoded varint.
///
/// # Panics
///
/// If `src` ends before the varint is complete (like Java's `BufferUnderflowException`).
pub fn get_var_long(src: &mut &[u8]) -> i64 {
    try_get_var_long(src).expect("buffer underflow reading varint")
}

/// Encodes a long integer in a variable-length encoding, 7 bits per byte, appending to `out`.
pub fn put_var_long(v: i64, out: &mut Vec<u8>) {
    let mut v = v as u64;
    loop {
        let bits = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(bits);
            return;
        }
        out.push(bits | 0x80);
    }
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
