//! HTTP bearer authentication that runs before rmcp service selection.

use crate::config::BearerDigest;
use axum::http::{HeaderMap, header::AUTHORIZATION};
use sha2::{Digest as _, Sha256};
use subtle::{Choice, ConditionallySelectable as _, ConstantTimeEq as _};

const MIN_BEARER_BYTES: usize = 32;
const MAX_BEARER_BYTES: usize = 512;

pub(crate) fn authenticate(headers: &HeaderMap, expected: &[BearerDigest]) -> Option<usize> {
    let values = headers.get_all(AUTHORIZATION);
    let mut values = values.iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let mut parts = value.split(' ');
    let scheme = parts.next()?;
    let token = parts.next()?;
    if parts.next().is_some()
        || !scheme.eq_ignore_ascii_case("bearer")
        || !well_formed_bearer_token(token)
    {
        return None;
    }

    let candidate = bearer_digest(token.as_bytes());
    let mut matched_index = 0_u8;
    let mut any_match = Choice::from(0);
    for (index, configured) in expected.iter().enumerate() {
        let equal = configured.bytes().ct_eq(&candidate);
        let index = u8::try_from(index).expect("configured client count is bounded");
        matched_index = u8::conditional_select(&matched_index, &index, equal);
        any_match |= equal;
    }
    bool::from(any_match).then_some(usize::from(matched_index))
}

pub(crate) fn bearer_digest(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

fn well_formed_bearer_token(token: &str) -> bool {
    if token.len() < MIN_BEARER_BYTES || token.len() > MAX_BEARER_BYTES {
        return false;
    }
    let unpadded = token.trim_end_matches('=');
    !unpadded.is_empty()
        && unpadded.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
}

#[cfg(test)]
mod tests {
    use super::well_formed_bearer_token;

    #[test]
    fn bearer_token_uses_only_the_rfc_6750_b64token_grammar() {
        assert!(well_formed_bearer_token(
            "abcdefghijklmnopqrstuvwxyz-._~+/0123456789=="
        ));
        for token in [
            "short",
            "abcdefghijklmnopqrstuvwxyz012345:6789",
            "abcdefghijklmnopqrstuvwxyz012=3456789",
            "================================",
            "abcdefghijklmnopqrstuvwxyz 0123456789",
        ] {
            assert!(!well_formed_bearer_token(token), "accepted {token:?}");
        }
    }
}
