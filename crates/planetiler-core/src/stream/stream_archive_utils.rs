//! Port of `stream/StreamArchiveUtils.java`.

use std::path::{Path, PathBuf};

use crate::archive::tile_archive_config::{ArchiveOptions, Format};

/// Path of the `index`-th output file: the base path for 0, else the base path with `index` appended.
pub fn construct_indexed_path(base_path: &Path, index: usize) -> PathBuf {
    if index == 0 { base_path.to_path_buf() } else { PathBuf::from(format!("{}{index}", base_path.display())) }
}

/// Reads a string option that may be wrapped in single quotes (to preserve whitespace) and contain Java escape
/// sequences like `\n` or `\t`.
pub fn get_escaped_string(
    options: &ArchiveOptions,
    format: Format,
    key: &str,
    description_prefix: &str,
    default_value: &str,
    examples: &[&str],
) -> String {
    let cli_key = format!("{}_{key}", format.id());
    let full_description = format!(
        "{description_prefix} - pass it as option: {}, or append to the file: {}",
        examples.iter().map(|e| format!("{cli_key}={}", escape_java(e))).collect::<Vec<_>>().join(" | "),
        examples.iter().map(|e| format!("?{key}={}", escape_java_uri(e))).collect::<Vec<_>>().join(" | "),
    );
    let raw = options.get_string(key, &full_description, default_value);
    // allow values to be wrapped by single quotes => allows to pass a space which otherwise gets trimmed
    let unquoted = if raw.len() >= 3 && raw.starts_with('\'') && raw.ends_with('\'') {
        &raw[1..raw.len() - 1]
    } else {
        raw.as_str()
    };
    translate_escapes(unquoted)
}

/// Port of Java `String.translateEscapes()`.
pub fn translate_escapes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '\\' || i >= chars.len() {
            out.push(c);
            continue;
        }
        let next = chars[i];
        i += 1;
        match next {
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            's' => out.push(' '),
            't' => out.push('\t'),
            '\'' | '"' | '\\' => out.push(next),
            '\n' => {}
            '\r' => {
                if i < chars.len() && chars[i] == '\n' {
                    i += 1;
                }
            }
            '0'..='7' => {
                let limit = if next <= '3' { 2 } else { 1 };
                let mut code = next.to_digit(8).unwrap_or(0);
                let mut j = 0;
                while j < limit && i < chars.len() && ('0'..='7').contains(&chars[i]) {
                    code = code * 8 + chars[i].to_digit(8).unwrap_or(0);
                    i += 1;
                    j += 1;
                }
                out.push(char::from_u32(code).unwrap_or('?'));
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn escape_java(s: &str) -> String {
    let s = if s.trim() != s { format!("'{s}'") } else { s.to_string() };
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

fn escape_java_uri(s: &str) -> String {
    let mut out = String::new();
    for b in escape_java(s).bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => out.push(b as char),
            b' ' => out.push('+'),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encoding with padding (Java `Base64.getEncoder()`).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(BASE64_CHARS[(n >> 18) as usize & 63] as char);
        out.push(BASE64_CHARS[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { BASE64_CHARS[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { BASE64_CHARS[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Lowercase hex encoding (Java `HexFormat.of().formatHex`).
pub fn hex_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_escaped_string() {
        for (input, output) in [("a", "a"), ("'a'", "a"), ("' '", " "), ("'\\n'", "\n")] {
            let options = ArchiveOptions::of_pairs(&[("key", input)]);
            assert_eq!(output, get_escaped_string(&options, Format::Csv, "key", "descr.", "ex", &["\n", " "]));
        }
    }

    #[test]
    fn test_construct_indexed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.test");
        assert_eq!(base, construct_indexed_path(&base, 0));
        assert_eq!(tmp.path().join("base.test1"), construct_indexed_path(&base, 1));
        assert_eq!(tmp.path().join("base.test13"), construct_indexed_path(&base, 13));
    }

    #[test]
    fn test_base64() {
        assert_eq!("AA==", base64_encode(&[0]));
        assert_eq!("AAE=", base64_encode(&[0, 1]));
        assert_eq!("AAEC", base64_encode(&[0, 1, 2]));
        assert_eq!("", base64_encode(&[]));
        assert_eq!("0001ff", hex_encode(&[0, 1, 255]));
    }
}
