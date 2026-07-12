//! Minimal Cookie-header parsing: just enough to check whether a cookie of
//! a given name is present (see [`has_cookie`]). Stadhouder never reads or
//! trusts a cookie's VALUE here, only its presence, as a cheap "this
//! request came from something that already established a session"
//! signal - see `Config::cookie_name`.
/// Whether `header_value` (the raw `Cookie:` header, e.g. `"a=1; b=2"`)
/// contains a cookie named `name`. `None` (no Cookie header at all) is
/// always `false`.
pub fn has_cookie(header_value: Option<&str>, name: &str) -> bool {
    let Some(header_value) = header_value else {
        return false;
    };
    header_value.split(';').any(|pair| {
        pair.split_once('=')
            .map(|(k, _)| k.trim() == name)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_cookie_among_several() {
        assert!(has_cookie(Some("a=1; keyscarf_session=abc; b=2"), "keyscarf_session"));
    }

    #[test]
    fn missing_header_is_false() {
        assert!(!has_cookie(None, "keyscarf_session"));
    }

    #[test]
    fn absent_cookie_is_false() {
        assert!(!has_cookie(Some("a=1; b=2"), "keyscarf_session"));
    }

    #[test]
    fn tolerates_no_leading_space() {
        assert!(has_cookie(Some("keyscarf_session=abc;b=2"), "keyscarf_session"));
    }

    #[test]
    fn name_must_match_exactly_not_as_a_substring() {
        assert!(!has_cookie(Some("keyscarf_session_extra=abc"), "keyscarf_session"));
        assert!(!has_cookie(Some("extra_keyscarf_session=abc"), "keyscarf_session"));
    }

    #[test]
    fn a_bare_token_with_no_equals_is_not_matched() {
        assert!(!has_cookie(Some("keyscarf_session"), "keyscarf_session"));
    }
}
