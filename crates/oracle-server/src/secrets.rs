//! Secrets are refused on write (plan.oracle.parity secrets-on-write; the competitor map scored us "missing": Hindsight scans on
//! write, ours was a standing rule for seats). Oracle never forgets: a record is signed, chained and mirrored, so a key written
//! once is a key leaked for good. Every live write is checked here before it is stored; a match refuses the write and names the
//! kind and the character where it starts, never the secret itself. The patterns are the high-precision ones (a provider prefix
//! and a fixed shape, or a PEM private-key header): a false refusal costs a seat one retry, a false pass costs a rotation.
//! A seat that is sure a match is not a secret (a published test vector, say) tags the write `secret-scan:allow`.

use regex::Regex;
use std::sync::LazyLock;

pub const ALLOW_TAG: &str = "secret-scan:allow";

static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("a private key", r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED |PGP )?PRIVATE KEY(?: BLOCK)?-----"),
        ("an AWS access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("a GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{60,})\b"),
        ("a Slack token", r"\bxox[abposr]-[0-9A-Za-z-]{10,}"),
        ("a Slack webhook", r"https://hooks\.slack\.com/services/T[0-9A-Z]{6,}/B[0-9A-Z]{6,}/[0-9A-Za-z]{20,}"),
        ("an Anthropic API key", r"\bsk-ant-(?:api|admin)\d{2}-[A-Za-z0-9_-]{40,}"),
        ("an OpenAI API key", r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20}T3BlbkFJ[A-Za-z0-9_-]{20,}|\bsk-proj-[A-Za-z0-9_-]{60,}"),
        ("a Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
        ("a Stripe live key", r"\b(?:sk|rk)_live_[0-9A-Za-z]{24,}\b"),
        ("a Telegram bot token", r"\b\d{8,10}:AA[0-9A-Za-z_-]{33}\b"),
        ("a JSON web token", r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{20,}"),
    ]
    .into_iter()
    .map(|(kind, re)| (kind, Regex::new(re).expect("secret pattern compiles")))
    .collect()
});

/// What a match is: its kind and the character offset where it starts in the text it was found in.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: &'static str,
    pub at: usize,
}

/// The first secret-shaped run in `text`, if any. A run of one character repeated (a redaction mark such as `AKIAXXXX…`) is not a key.
pub fn find(text: &str) -> Option<Finding> {
    PATTERNS.iter().find_map(|(kind, re)| {
        re.find_iter(text)
            .find(|m| !redacted(m.as_str()))
            .map(|m| Finding { kind, at: text[..m.start()].chars().count() })
    })
}

fn redacted(s: &str) -> bool {
    let tail: Vec<char> = s.chars().rev().take(12).collect();
    tail.windows(2).all(|w| w[0] == w[1])
}

/// The refusal for a write whose content or cargo holds a secret, or None when it is clean or tagged `secret-scan:allow`.
pub fn refusal(content: &str, cargo: Option<&str>, tags: &[String]) -> Option<String> {
    if tags.iter().any(|t| t == ALLOW_TAG) {
        return None;
    }
    let (place, f) = find(content).map(|f| ("the text", f)).or_else(|| cargo.and_then(find).map(|f| ("the cargo", f)))?;
    Some(format!(
        "{place} holds what looks like {} at character {}; Oracle keeps every record for good, so nothing was stored. Remove it (rotate it if it was real), or tag {ALLOW_TAG} if it is not a secret",
        f.kind, f.at
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built at run time so this file holds no literal that a scanner, ours or another, would flag.
    fn k(prefix: &str, body: &str, n: usize) -> String {
        format!("{prefix}{}", &body.repeat(n.div_ceil(body.len()))[..n])
    }

    #[test]
    fn each_kind_is_found_and_named_by_its_offset() {
        let cases = [
            (format!("-----BEGIN OPENSSH {}-----", "PRIVATE KEY"), "a private key"),
            (k("AKIA", "Q7ZJ3K9D2M", 16), "an AWS access key id"),
            (k("ghp_", "a1B2c3D4e5", 36), "a GitHub token"),
            (k("xoxb-", "1234-abcd-", 30), "a Slack token"),
            (k("sk-ant-api03-", "aB3_dE6-gH", 90), "an Anthropic API key"),
            (k("sk-proj-", "Zx9_Qw-1Ty", 120), "an OpenAI API key"),
            (k("AIza", "Sy7_kd-3Lp", 35), "a Google API key"),
            (k("sk_live_", "4eC39HqLyj", 24), "a Stripe live key"),
            (format!("123456789:AA{}", &"h3K_9dL-2q".repeat(4)[..33]), "a Telegram bot token"),
            (format!("eyJ{}.eyJ{}.{}", "hbGciOiJIUzI1", "zdWIiOiIxMjM0", "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV"), "a JSON web token"),
        ];
        for (secret, kind) in &cases {
            let text = format!("héllo {secret} done");
            assert_eq!(find(&text), Some(Finding { kind, at: 6 }), "{kind}: char offset, not bytes");
        }
    }

    #[test]
    fn ordinary_words_redactions_and_prefixes_alone_pass() {
        for clean in [
            "the AKIA prefix names an AWS key; ghp_ a GitHub one; sk-ant- an Anthropic one",
            &k("AKIA", "X", 16),
            &k("sk-ant-api03-", "x", 90),
            "a hex commit 5c22b0ddf75676df77a2c178999c2a9f48b00e9e and an id OBS-oracle-20001027-e843697beea3",
            "-----BEGIN PUBLIC KEY-----",
            "ox say oracle \"@emmy release dev\" --tag ask:reply",
        ] {
            assert_eq!(find(clean), None, "{clean}");
        }
    }

    #[test]
    fn a_refusal_names_the_place_and_kind_never_the_secret_and_the_allow_tag_passes() {
        let key = k("ghp_", "a1B2c3D4e5", 36);
        let why = refusal("clean", Some(&format!("{{\"token\":\"{key}\"}}")), &[]).expect("cargo is scanned");
        assert!(why.starts_with("the cargo holds what looks like a GitHub token"), "{why}");
        assert!(!why.contains(&key[4..]), "the refusal never repeats the secret");
        assert!(refusal(&key, None, &[ALLOW_TAG.to_string()]).is_none());
        assert!(refusal("nothing here", Some("{}"), &[]).is_none());
    }
}
