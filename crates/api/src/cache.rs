//! Cache validators.
//!
//! A CDN in front is the scaling plan for a public read service, so validators are not a nicety:
//! they decide whether a cache can answer at all.

use axum::http::HeaderMap;
use axum::http::header::IF_NONE_MATCH;

/// Whether the request's `If-None-Match` covers `etag`, in which case the answer is 304.
///
/// Handles the list form and `*`, and compares weakly — a `W/` prefix on either side is ignored.
/// Weak comparison is the only kind `If-None-Match` is defined to use.
#[must_use]
pub fn is_fresh(headers: &HeaderMap, etag: &str) -> bool {
    let Some(header) = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    header
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || strip_weak(candidate) == strip_weak(etag))
}

fn strip_weak(etag: &str) -> &str {
    etag.strip_prefix("W/").unwrap_or(etag)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            IF_NONE_MATCH,
            HeaderValue::from_str(value).expect("valid header"),
        );
        headers
    }

    #[test]
    fn an_exact_match_is_fresh() {
        assert!(is_fresh(&headers("\"abc\""), "\"abc\""));
    }

    #[test]
    fn a_list_is_searched() {
        assert!(is_fresh(&headers("\"x\", \"abc\", \"y\""), "\"abc\""));
    }

    #[test]
    fn a_star_matches_anything() {
        assert!(is_fresh(&headers("*"), "\"abc\""));
    }

    #[test]
    fn weakness_is_ignored_on_either_side() {
        assert!(is_fresh(&headers("W/\"abc\""), "\"abc\""));
        assert!(is_fresh(&headers("\"abc\""), "W/\"abc\""));
    }

    #[test]
    fn a_different_validator_is_stale() {
        assert!(!is_fresh(&headers("\"abc\""), "\"abd\""));
        assert!(!is_fresh(&HeaderMap::new(), "\"abc\""));
    }
}
