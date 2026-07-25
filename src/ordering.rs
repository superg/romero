use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::Path;

pub(crate) fn text(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

pub(crate) fn os(left: &OsStr, right: &OsStr) -> Ordering {
    text(&left.to_string_lossy(), &right.to_string_lossy()).then_with(|| left.cmp(right))
}

pub(crate) fn path(left: &Path, right: &Path) -> Ordering {
    os(left.as_os_str(), right.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_order_is_case_insensitive_with_a_deterministic_tie_breaker() {
        let mut values = ["Zulu", "apple", "Beta", "alpha", "ALPHA"];

        values.sort_by(|left, right| text(left, right));

        assert_eq!(values, ["ALPHA", "alpha", "apple", "Beta", "Zulu"]);
    }
}
