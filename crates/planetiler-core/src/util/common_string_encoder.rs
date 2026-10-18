//! Port of `util/CommonStringEncoder.java`: a thread-safe utility for compressing commonly-used strings (i.e. layer
//! name, tag attributes) into small integer IDs.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

/// Assigns sequential integer IDs to unique strings, up to `max_strings`.
pub struct CommonStringEncoder {
    max_strings: usize,
    string_to_id: RwLock<HashMap<Arc<str>, u32>>,
    id_to_string: Box<[OnceLock<Arc<str>>]>,
}

impl CommonStringEncoder {
    pub fn new(max_strings: usize) -> CommonStringEncoder {
        CommonStringEncoder {
            max_strings,
            string_to_id: RwLock::new(HashMap::new()),
            id_to_string: (0..max_strings).map(|_| OnceLock::new()).collect(),
        }
    }

    /// Returns the string for `id`, or an error if there is no value for `id`.
    pub fn decode(&self, id: u32) -> anyhow::Result<&Arc<str>> {
        self.id_to_string.get(id as usize).and_then(OnceLock::get).ok_or_else(|| anyhow::anyhow!("No string for {id}"))
    }

    /// Returns all strings in the order they were first encoded, for serialization purposes.
    pub fn get_strings_in_order(&self) -> Vec<Arc<str>> {
        // hold the lock so that we see a consistent prefix
        let map = self.string_to_id.read();
        let n = map.len();
        self.id_to_string[..n].iter().filter_map(|s| s.get().cloned()).collect()
    }

    /// Returns a unique ID for `string` that can be converted back with [`CommonStringEncoder::decode`], or an error if
    /// called for too many distinct values.
    pub fn encode(&self, string: &str) -> anyhow::Result<u32> {
        if let Some(&id) = self.string_to_id.read().get(string) {
            return Ok(id);
        }
        let mut map = self.string_to_id.write();
        if let Some(&id) = map.get(string) {
            return Ok(id);
        }
        let id = map.len();
        if id >= self.max_strings {
            anyhow::bail!("Too many strings");
        }
        let s: Arc<str> = Arc::from(string);
        let _ = self.id_to_string[id].set(s.clone());
        map.insert(s, id as u32);
        Ok(id as u32)
    }
}

/// Variant of [`CommonStringEncoder`] that uses a single byte for string IDs (max 256 strings).
pub struct AsByte {
    encoder: CommonStringEncoder,
}

impl Default for AsByte {
    fn default() -> Self {
        Self::new()
    }
}

impl AsByte {
    pub fn new() -> AsByte {
        AsByte { encoder: CommonStringEncoder::new(256) }
    }

    pub fn decode(&self, id: u8) -> anyhow::Result<&Arc<str>> {
        self.encoder.decode(id as u32)
    }

    pub fn encode(&self, string: &str) -> anyhow::Result<u8> {
        Ok(self.encoder.encode(string)? as u8)
    }

    /// Returns all strings in the order they were first encoded, for serialization purposes.
    pub fn get_strings_in_order(&self) -> Vec<Arc<str>> {
        self.encoder.get_strings_in_order()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_byte() {
        let enc = AsByte::new();
        let a = enc.encode("a").unwrap();
        let b = enc.encode("b").unwrap();
        assert_eq!("a", &**enc.decode(a).unwrap());
        assert_eq!(a, enc.encode("a").unwrap());
        assert_eq!("b", &**enc.decode(b).unwrap());
        assert!(enc.decode(b + 1).is_err());
        assert_eq!(vec![Arc::from("a"), Arc::from("b")], enc.get_strings_in_order());
    }

    #[test]
    fn test_round_trip_integer() {
        let enc = CommonStringEncoder::new(100_000);
        let a = enc.encode("a").unwrap();
        let b = enc.encode("b").unwrap();
        assert_eq!("a", &**enc.decode(a).unwrap());
        assert_eq!(a, enc.encode("a").unwrap());
        assert_eq!("b", &**enc.decode(b).unwrap());
        assert!(enc.decode(b + 1).is_err());
    }

    #[test]
    fn test_byte_limits_to_max() {
        let enc = AsByte::new();
        for i in 0..=255 {
            let string = i.to_string();
            let encoded = enc.encode(&string).unwrap();
            assert_eq!(string, &**enc.decode(encoded).unwrap());
        }
        assert!(enc.encode("too many").is_err());
    }

    #[test]
    fn test_int_does_not_limit_to_250() {
        let enc = CommonStringEncoder::new(100_000);
        for i in 0..100_000 {
            let string = i.to_string();
            let encoded = enc.encode(&string).unwrap();
            assert_eq!(string, &**enc.decode(encoded).unwrap());
        }
        assert!(enc.encode("too many").is_err());
    }
}
