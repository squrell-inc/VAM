//! Canonical form of the run receipt (E12).
//!
//! The node signs the canonical bytes of the `receipt` object, and an offline
//! verifier recomputes them from the JSON it was given. Both sides work on
//! `serde_json::Value`, never on the generated proto types, so a field a newer
//! node adds cannot break an older verifier.
//!
//! Canonical form:
//! - object keys sorted by their UTF-8 bytes, recursively (this workspace's
//!   `serde_json` keeps insertion order, so the sort is explicit);
//! - compact output, no whitespace;
//! - strings escaped as `serde_json` does (control characters, `"` and `\`;
//!   other characters, including non-ASCII, are written as UTF-8);
//! - numbers are integers within +/- (2^53 - 1) only: no floats, no exponents,
//!   so a JavaScript verifier reads the same value;
//! - nesting deeper than [`MAX_DEPTH`] is refused.

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Receipt format and signature domain.
pub const RECEIPT_VERSION: &str = "vam.receipt.v1";

/// Deepest nesting a receipt may have.
pub const MAX_DEPTH: usize = 64;

/// Largest integer magnitude a JavaScript verifier reads exactly.
const MAX_SAFE_INT: i64 = (1 << 53) - 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CanonError {
    #[error("not valid JSON: {0}")]
    Parse(String),
    #[error("number {0} is not an integer (floats and exponents are not allowed)")]
    NotInteger(String),
    #[error("integer {0} is outside +/-(2^53-1)")]
    IntegerRange(String),
    #[error("nesting deeper than {MAX_DEPTH}")]
    TooDeep,
    #[error("not valid hex")]
    Hex,
}

/// Canonical bytes of `value`.
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, CanonError> {
    let mut out = String::new();
    write_value(value, 0, &mut out)?;
    Ok(out.into_bytes())
}

/// Canonical bytes of the JSON text `json`.
pub fn canonical_from_json(json: &[u8]) -> Result<Vec<u8>, CanonError> {
    let value: Value =
        serde_json::from_slice(json).map_err(|e| CanonError::Parse(e.to_string()))?;
    canonical_bytes(&value)
}

fn write_value(value: &Value, depth: usize, out: &mut String) -> Result<(), CanonError> {
    if depth > MAX_DEPTH {
        return Err(CanonError::TooDeep);
    }
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if !(-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&i) {
                    return Err(CanonError::IntegerRange(n.to_string()));
                }
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                // Positive and above i64::MAX, so above the safe range too.
                return Err(CanonError::IntegerRange(u.to_string()));
            } else {
                return Err(CanonError::NotInteger(n.to_string()));
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, depth + 1, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(&map[key], depth + 1, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_string(s: &str, out: &mut String) {
    // Serialising a `&str` cannot fail.
    out.push_str(&serde_json::to_string(s).expect("string serialises"));
}

/// `sha256:<lowercase hex>` of `bytes`.
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_encode(&Sha256::digest(bytes)))
}

/// Lowercase hex.
pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decodes lowercase or uppercase hex.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, CanonError> {
    fn nibble(c: u8) -> Result<u8, CanonError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(CanonError::Hex),
        }
    }
    let b = s.as_bytes();
    if b.len() % 2 == 1 {
        return Err(CanonError::Hex);
    }
    b.chunks(2)
        .map(|p| Ok((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(json: &str) -> Result<String, CanonError> {
        canonical_from_json(json.as_bytes()).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn golden_canonical_bytes() {
        let got = canon(r#"{ "b": 1, "a": { "d": [true, null, "x"], "c": -2 }, "e": "hé\n\"" }"#)
            .unwrap();
        assert_eq!(
            got,
            "{\"a\":{\"c\":-2,\"d\":[true,null,\"x\"]},\"b\":1,\"e\":\"h\u{e9}\\n\\\"\"}"
        );
    }

    #[test]
    fn key_order_and_whitespace_do_not_change_the_bytes() {
        let a = canon(r#"{"z":1,"a":[{"y":2,"x":3}]}"#).unwrap();
        let b = canon("{\n  \"a\" : [ { \"x\":3, \"y\":2 } ],\n\t\"z\":1 }").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, r#"{"a":[{"x":3,"y":2}],"z":1}"#);
    }

    #[test]
    fn keys_sort_by_utf8_bytes_not_locale() {
        assert_eq!(
            canon(r#"{"b":1,"B":2,"a":3,"é":4,"Z":5}"#).unwrap(),
            "{\"B\":2,\"Z\":5,\"a\":3,\"b\":1,\"\u{e9}\":4}"
        );
    }

    #[test]
    fn floats_and_exponents_are_refused() {
        for bad in ["1.5", "1.0", "1e2", "-0.0", "1E-2"] {
            let json = format!(r#"{{"n":{bad}}}"#);
            assert!(
                matches!(canon(&json), Err(CanonError::NotInteger(_))),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn integers_outside_the_safe_range_are_refused() {
        assert_eq!(
            canon(r#"{"n":9007199254740991}"#).unwrap(),
            r#"{"n":9007199254740991}"#
        );
        assert_eq!(
            canon(r#"{"n":-9007199254740991}"#).unwrap(),
            r#"{"n":-9007199254740991}"#
        );
        assert!(matches!(
            canon(r#"{"n":9007199254740992}"#),
            Err(CanonError::IntegerRange(_))
        ));
        assert!(matches!(
            canon(r#"{"n":-9007199254740992}"#),
            Err(CanonError::IntegerRange(_))
        ));
        assert!(matches!(
            canon(r#"{"n":18446744073709551615}"#),
            Err(CanonError::IntegerRange(_))
        ));
    }

    #[test]
    fn deep_nesting_is_refused() {
        let deep = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        assert_eq!(canon(&deep), Err(CanonError::TooDeep));
    }

    #[test]
    fn bad_json_is_a_parse_error() {
        assert!(matches!(canon("{"), Err(CanonError::Parse(_))));
    }

    #[test]
    fn digest_is_sha256_hex_with_prefix() {
        assert_eq!(
            digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_decode("00ABff").unwrap(), vec![0x00, 0xab, 0xff]);
        assert_eq!(hex_decode("0"), Err(CanonError::Hex));
        assert_eq!(hex_decode("zz"), Err(CanonError::Hex));
    }
}
