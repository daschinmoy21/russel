//! Shared token helpers: constant-time comparison, normalization, and
//! HTTP-header-safety validation for API / agent bearer tokens.
//!
//! Single source of truth for the security-sensitive token logic previously
//! duplicated between `russel-ctrl` and `russel-agent`. The constant-time
//! comparison is pure Rust and portable — no unix-only dependencies.

/// Minimum accepted length for tokens after trim.
///
/// Floor is 32 **ASCII** characters so weak tokens like `"a"` are rejected.
/// Prefer `openssl rand -hex 32` (64 hex chars / 256 bits) for production.
pub const MIN_TOKEN_LEN: usize = 32;

/// Constant-time token comparison to avoid timing side-channels.
///
/// Always walks `max(a.len(), b.len())` bytes so the result does not leak the
/// input lengths. A length mismatch is folded into the accumulator as a
/// non-zero delta rather than returned early.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    // Length mismatch must always contribute a nonzero delta. Narrowing
    // `(a.len() ^ b.len()) as u8` drops high bits (e.g. len 1 vs 257 → 0).
    let mut diff: u8 = u8::from(a.len() != b.len());
    for i in 0..max_len {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Pure token normalize: unset/blank/whitespace → None.
///
/// Does **not** enforce min length / charset — callers apply
/// [`check_token_min_length`] at startup when a token is present so short or
/// non-header-safe secrets fail closed.
pub fn normalize_token(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether `token` can appear in an HTTP `Authorization` header value.
///
/// Matches what the CLI needs: `HeaderValue` accepts visible ASCII (0x20..=0x7E)
/// and HTAB. Multibyte Unicode and control bytes are rejected so the process
/// never starts with a token clients cannot send.
pub fn token_is_http_header_safe(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
}

/// Reject tokens that are too short or cannot be sent as a Bearer header value.
///
/// Call this at process startup whenever [`normalize_token`] returns `Some`.
///
/// Checks (in order):
/// 1. HTTP header-safe charset (ASCII visible / HTAB) — same constraint as the CLI
/// 2. Length ≥ [`MIN_TOKEN_LEN`] (byte length; equivalent to char count after 1)
pub fn check_token_min_length(token: &str) -> Result<(), String> {
    if !token_is_http_header_safe(token) {
        return Err(
            "RUSSEL_API_TOKEN must be printable ASCII only so it can be sent in an \
             HTTP Authorization header (the CLI rejects non-header-safe tokens). \
             Generate a strong token with: openssl rand -hex 32"
                .to_string(),
        );
    }
    if token.len() < MIN_TOKEN_LEN {
        Err(format!(
            "RUSSEL_API_TOKEN must be at least {MIN_TOKEN_LEN} characters after trim \
             (got {}). Generate a strong token with: openssl rand -hex 32",
            token.len()
        ))
    } else {
        Ok(())
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header value.
///
/// The `Bearer` scheme is matched case-insensitively (RFC 7235), so `Bearer`,
/// `bearer`, `BEARER`, … are all accepted. The remainder is trimmed and an
/// empty token reports `None`.
pub fn bearer_token_from_header(header: &str) -> Option<&str> {
    if !header
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "))
    {
        return None;
    }
    let token = header.get(7..)?.trim();
    if token.is_empty() { None } else { Some(token) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_identical() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_different_same_length() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"\x00\x01", b"\x00\x00"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"hello", b"hello!"));
        assert!(!constant_time_eq(b"a", b""));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"abcdefghij", b"abcde"));
    }

    #[test]
    fn constant_time_eq_zeroed_suffix_does_not_match() {
        // Shorter slice that is a prefix of a zero-padded longer one must NOT
        // match — the length mismatch is folded into the diff.
        assert!(!constant_time_eq(b"abc", b"abc\0\0"));
        // Length XOR truncated to u8 would be 0 for 1 vs 257; still rejected.
        let short = [0u8; 1];
        let long = [0u8; 257];
        assert!(!constant_time_eq(&short, &long));
    }

    #[test]
    fn normalize_token_roundtrip() {
        assert_eq!(normalize_token(None), None);
        assert_eq!(normalize_token(Some("")), None);
        assert_eq!(normalize_token(Some("   ")), None);
        assert_eq!(normalize_token(Some("secret")), Some("secret".to_string()));
        assert_eq!(
            normalize_token(Some("  secret  ")),
            Some("secret".to_string())
        );
    }

    #[test]
    fn header_safe_charset() {
        assert!(token_is_http_header_safe("abcXYZ019_-"));
        assert!(token_is_http_header_safe("with\ttab"));
        assert!(!token_is_http_header_safe("no\nnewline"));
        assert!(!token_is_http_header_safe("é"));
        assert!(!token_is_http_header_safe("🔐"));
    }

    #[test]
    fn min_length_rejects_short_and_non_ascii() {
        assert!(check_token_min_length("a").is_err());
        assert!(check_token_min_length(&"x".repeat(MIN_TOKEN_LEN - 1)).is_err());
        let err = check_token_min_length("a").unwrap_err();
        assert!(err.contains(&MIN_TOKEN_LEN.to_string()), "err={err}");
        assert!(err.contains("openssl rand -hex 32"), "err={err}");

        // Non-ASCII rejected even when the UTF-8 byte length meets the floor.
        let unicode = "é".repeat(16);
        assert!(unicode.len() >= MIN_TOKEN_LEN);
        assert!(check_token_min_length(&unicode).is_err());
    }

    #[test]
    fn min_length_accepts_floor_and_longer() {
        assert!(check_token_min_length(&"a".repeat(MIN_TOKEN_LEN)).is_ok());
        assert!(check_token_min_length(&"b".repeat(64)).is_ok());
    }

    #[test]
    fn bearer_prefix_case_insensitive() {
        assert_eq!(bearer_token_from_header("Bearer tok"), Some("tok"));
        assert_eq!(bearer_token_from_header("bearer tok"), Some("tok"));
        assert_eq!(bearer_token_from_header("BEARER tok"), Some("tok"));
        assert_eq!(bearer_token_from_header("bEaReR tok"), Some("tok"));
        assert_eq!(bearer_token_from_header("Bearer   tok  "), Some("tok"));
    }

    #[test]
    fn bearer_prefix_rejects_other_schemes_and_empty() {
        assert_eq!(bearer_token_from_header("Basic tok"), None);
        assert_eq!(bearer_token_from_header("Bearer"), None);
        assert_eq!(bearer_token_from_header("Bearer "), None);
        assert_eq!(bearer_token_from_header("bearer \t "), None);
        assert_eq!(bearer_token_from_header(""), None);
    }
}
