//! How a seat or room handle is spelled in the server's own tables, and how text is escaped into the pages it serves:
//! one rule each, so what one table accepts another never refuses (code audit, 4 Nov 2026: five identical copies of the
//! handle rule, and two HTML escapes of which only one escaped `'`). The statechart writer (lifecycle.rs) keeps its own
//! XML escape on purpose: it writes into XML comments, where a character reference is not decoded and `'` must stay.

/// A handle as the server's tables hold it (aliases, private rooms, retention, seat rooms, `oracle up`): 1 to 64 of
/// lowercase letters, digits, `-` and `_`, starting with a letter or digit.
///
/// The protocol's own rule (`oracle_proto`, on every envelope) is wider: it also lets a handle start with `-` or `_`.
/// That cannot be narrowed without un-verifying signed history (a live store holds records by `__riteprobe__`), so the
/// wider rule stays for what was signed and this one governs what the server writes into its tables.
pub fn handle_ok(h: &str) -> bool {
    let b = h.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// Text made safe for HTML element content and attribute values, double- or single-quoted (the public record page, the
/// OAuth consent page).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_handle_starts_with_a_letter_or_digit() {
        for ok in ["rocky", "dot-game", "a", "7x", "seat_2", &"a".repeat(64)] {
            assert!(handle_ok(ok), "{ok}");
        }
        for bad in ["", "_x", "-x", "Rocky", "a b", "a.b", "é", &"a".repeat(65)] {
            assert!(!handle_ok(bad), "{bad}");
        }
    }

    #[test]
    fn escaping_covers_both_quote_kinds() {
        assert_eq!(esc("<a href=\"x\">'&"), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;");
        assert_eq!(esc("plain words"), "plain words");
    }
}
