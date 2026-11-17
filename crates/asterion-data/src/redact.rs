use std::{env, fmt};

use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::error::DataError;

pub const MASK: &str = "****";

/// data.go.kr service key. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceKey(String);

impl ServiceKey {
    pub const ENV_VAR: &'static str = "DATA_GO_KR_SERVICE_KEY";

    /// Accepts either the portal's "Decoding" key or its percent-encoded
    /// "Encoding" key; the latter is decoded so the HTTP client encodes it once.
    pub fn new(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.contains('%') {
            ServiceKey(percent_decode_str(raw).decode_utf8_lossy().into_owned())
        } else {
            ServiceKey(raw.to_owned())
        }
    }

    pub fn from_env() -> Result<Self, DataError> {
        match env::var(Self::ENV_VAR) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(&value)),
            _ => Err(DataError::MissingServiceKey(Self::ENV_VAR)),
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Masks the key (raw and percent-encoded) and any `serviceKey=` query value.
    pub fn redact(&self, text: &str) -> String {
        redact_secret(text, &self.0)
    }
}

impl fmt::Debug for ServiceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceKey({MASK})")
    }
}

pub fn redact_secret(text: &str, secret: &str) -> String {
    let mut out = text.to_owned();
    if !secret.is_empty() {
        out = out.replace(secret, MASK);
        let encoded = utf8_percent_encode(secret, NON_ALPHANUMERIC).to_string();
        out = out.replace(&encoded, MASK);
        // Lowercase hex escapes (%2b) are equally valid in URLs.
        out = out.replace(&encoded.to_ascii_lowercase(), MASK);
    }
    mask_query_param(&out, "serviceKey=")
}

fn mask_query_param(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let value_start = pos + marker.len();
        out.push_str(&rest[..value_start]);
        let tail = &rest[value_start..];
        let value_len = tail
            .find(|c: char| c == '&' || c == '"' || c == '\'' || c == ')' || c.is_whitespace())
            .unwrap_or(tail.len());
        if value_len > 0 {
            out.push_str(MASK);
        }
        rest = &tail[value_len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ab+cd/ef==";

    #[test]
    fn encoding_key_is_decoded() {
        assert_eq!(ServiceKey::new("ab%2Bcd%2Fef%3D%3D").expose(), KEY);
        assert_eq!(ServiceKey::new(&format!(" {KEY}\n")).expose(), KEY);
    }

    #[test]
    fn debug_hides_key() {
        assert_eq!(format!("{:?}", ServiceKey::new(KEY)), "ServiceKey(****)");
    }

    #[test]
    fn redacts_raw_encoded_and_query_forms() {
        let key = ServiceKey::new(KEY);
        assert_eq!(key.redact("key ab+cd/ef== end"), "key **** end");
        assert_eq!(
            key.redact("GET /x?serviceKey=ab%2Bcd%2Fef%3D%3D&pageNo=1"),
            "GET /x?serviceKey=****&pageNo=1"
        );
        assert_eq!(
            key.redact("url (https://h/p?a=1&serviceKey=other-key)"),
            "url (https://h/p?a=1&serviceKey=****)"
        );
        assert_eq!(key.redact("ab%2bcd%2fef%3d%3d"), "****");
    }

    #[test]
    fn text_without_secret_is_unchanged() {
        assert_eq!(redact_secret("plain text", "zzz"), "plain text");
        assert_eq!(redact_secret("serviceKey=&x", ""), "serviceKey=&x");
    }
}
