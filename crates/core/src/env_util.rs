//! Environment-variable parsing helpers shared across the workspace.

/// Parse a boolean-ish environment value.
///
/// Case-insensitive and trimmed:
/// - truthy: `1`, `true`, `yes`, `on`
/// - falsy: `0`, `false`, `no`, `off`, `disabled`
/// - anything else (including absent or blank) → `None`
///
/// Callers decide their own default when this returns `None`.
pub fn env_bool(raw: Option<&str>) -> Option<bool> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::env_bool;

    #[test]
    fn truthy_values_are_true() {
        for v in ["1", "true", "yes", "on", "TRUE", "Yes", "On", " on ", "1"] {
            assert_eq!(env_bool(Some(v)), Some(true), "expected true for {v:?}");
        }
    }

    #[test]
    fn falsy_values_are_false() {
        for v in [
            "0", "false", "no", "off", "disabled", "FALSE", "OFF", " No ",
        ] {
            assert_eq!(env_bool(Some(v)), Some(false), "expected false for {v:?}");
        }
    }

    #[test]
    fn unrecognized_and_absent_are_none() {
        for v in [
            None,
            Some(""),
            Some("   "),
            Some("maybe"),
            Some("allow"),
            Some("deny"),
        ] {
            assert_eq!(env_bool(v), None, "expected None for {v:?}");
        }
    }
}
