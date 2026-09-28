//! Non-secret settings CI may write into an env file with a deploy.
//!
//! `deploy staging <digest> KEY=VALUE ...` and `promote prod <digest>
//! KEY=VALUE ...` carry them. Only the keys in [`ALLOWED`] are accepted,
//! and each value is checked the way `athena` itself will read it, so a
//! setting the gate writes is one `athena` accepts. Secrets are never
//! among them: a person types those into the env file.
//!
//! Values are not secret, so they are logged and returned in the JSON
//! line, old and new, to make every change visible in the CI summary.

use crate::sandbox::MIN_TIMEOUT_SECS;
use url::Url;

/// The keys CI may set, and nothing else. Not `ATHENA_ADDR` or
/// `ATHENA_ALLOWED_HOSTS` (the gate's own health checks depend on them),
/// not secrets, and not anything the process loader reads.
pub const ALLOWED: [&str; 4] = [
    "OPEN_SANDBOX_URL",
    "ATHENA_SANDBOX_IMAGE",
    "ATHENA_SANDBOX_TIMEOUT_SECS",
    "AGENT_MODEL",
];

/// Longest value accepted for any key.
pub const MAX_VALUE_LEN: usize = 200;

/// One `KEY=VALUE` word, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub value: String,
}

/// Parse the words after the digest. Each must be an allowed `KEY=VALUE`,
/// and each key may appear once.
pub fn parse(words: &[&str]) -> Result<Vec<Setting>, String> {
    let mut settings: Vec<Setting> = Vec::new();
    for word in words {
        let setting = parse_one(word)?;
        if settings.iter().any(|s| s.key == setting.key) {
            return Err(format!("{} is set twice", setting.key));
        }
        settings.push(setting);
    }
    Ok(settings)
}

fn parse_one(word: &str) -> Result<Setting, String> {
    let (key, value) = word
        .split_once('=')
        .ok_or_else(|| format!("{word:?} is not KEY=VALUE"))?;
    let key = ALLOWED
        .into_iter()
        .find(|allowed| *allowed == key)
        .ok_or_else(|| format!("{key:?} is not a setting CI may change"))?;
    if value.is_empty() || value.len() > MAX_VALUE_LEN {
        return Err(format!("{key} must be 1 to {MAX_VALUE_LEN} bytes"));
    }
    // Nothing systemd's `EnvironmentFile=` or the gate's parser treats
    // specially: no quotes, `$`, `#`, `;`, `=`, backslashes or whitespace.
    let plain = value.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'+' | b'-')
    });
    if !plain {
        return Err(format!(
            "{key} may only hold letters, digits and . _ : / @ + -"
        ));
    }
    match key {
        "OPEN_SANDBOX_URL" => check_url(value)?,
        "ATHENA_SANDBOX_TIMEOUT_SECS" => check_timeout(value)?,
        _ => {}
    }
    Ok(Setting {
        key,
        value: value.to_string(),
    })
}

/// As `sandbox::Config::parse` reads it (`url` already refuses an http or
/// https URL without a host), plus no user or password, since the value
/// is logged.
fn check_url(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|e| format!("OPEN_SANDBOX_URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("OPEN_SANDBOX_URL must be http or https".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("OPEN_SANDBOX_URL must not carry a user or password".into());
    }
    Ok(())
}

fn check_timeout(value: &str) -> Result<(), String> {
    let secs: u64 = value
        .parse()
        .map_err(|_| "ATHENA_SANDBOX_TIMEOUT_SECS must be a number".to_string())?;
    if secs < MIN_TIMEOUT_SECS {
        return Err(format!(
            "ATHENA_SANDBOX_TIMEOUT_SECS must be at least {MIN_TIMEOUT_SECS}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(word: &str) -> Result<Setting, String> {
        parse(&[word]).map(|mut s| s.remove(0))
    }

    #[test]
    fn every_allowed_key_is_accepted_with_a_valid_value() {
        let words = [
            "OPEN_SANDBOX_URL=http://100.118.54.67:9090",
            "ATHENA_SANDBOX_IMAGE=ghcr.io/queryplanner/athena-sandbox@sha256:abc",
            "ATHENA_SANDBOX_TIMEOUT_SECS=60",
            "AGENT_MODEL=openai/gpt-5.6-luna",
        ];
        let settings = parse(&words).unwrap();
        let keys: Vec<_> = settings.iter().map(|s| s.key).collect();
        assert_eq!(keys, ALLOWED);
        assert_eq!(settings[0].value, "http://100.118.54.67:9090");
        assert_eq!(parse(&[]).unwrap(), []);
    }

    #[test]
    fn keys_outside_the_allowlist_are_refused() {
        for word in [
            "LD_PRELOAD=/tmp/x.so",
            "ATHENA_ADDR=0.0.0.0:80",
            "ATHENA_ALLOWED_HOSTS=evil",
            "OPENROUTER_API_KEY=sk",
            "OPEN_SANDBOX_API_KEY=k",
            "OPEN_SANDBOX_URL_X=http://h",
            "agent_model=x",
        ] {
            let err = one(word).unwrap_err();
            let refused = err.contains("is not a setting CI may change");
            assert!(refused, "{word}: {err}");
        }
    }

    #[test]
    fn a_word_without_equals_is_refused() {
        assert!(one("AGENT_MODEL").unwrap_err().contains("is not KEY=VALUE"));
    }

    #[test]
    fn a_key_may_appear_once() {
        let err = parse(&["AGENT_MODEL=a", "AGENT_MODEL=b"]).unwrap_err();
        assert_eq!(err, "AGENT_MODEL is set twice");
    }

    #[test]
    fn values_must_be_short_and_plain() {
        let long = format!("AGENT_MODEL={}", "a".repeat(MAX_VALUE_LEN + 1));
        let max = format!("AGENT_MODEL={}", "a".repeat(MAX_VALUE_LEN));
        assert!(one("AGENT_MODEL=").unwrap_err().contains("1 to 200 bytes"));
        assert!(one(&long).unwrap_err().contains("1 to 200 bytes"));
        assert!(one(&max).is_ok());
        for word in [
            "AGENT_MODEL=$HOME",
            "AGENT_MODEL=a\"b",
            "AGENT_MODEL=a'b",
            "AGENT_MODEL=a#b",
            "AGENT_MODEL=a;b",
            "AGENT_MODEL=a=b",
            "AGENT_MODEL=a\\b",
            "AGENT_MODEL=a%20b",
        ] {
            assert!(one(word).unwrap_err().contains("may only hold"), "{word}");
        }
    }

    #[test]
    fn the_sandbox_url_is_checked_as_athena_reads_it() {
        assert!(one("OPEN_SANDBOX_URL=https://sandbox.example:9090/api").is_ok());
        let cases = [
            ("OPEN_SANDBOX_URL=nope", "OPEN_SANDBOX_URL: "),
            ("OPEN_SANDBOX_URL=http://", "OPEN_SANDBOX_URL: empty host"),
            ("OPEN_SANDBOX_URL=ftp://h:21", "must be http or https"),
            (
                "OPEN_SANDBOX_URL=file:///etc/passwd",
                "must be http or https",
            ),
            ("OPEN_SANDBOX_URL=http://user@h:1", "must not carry a user"),
            (
                "OPEN_SANDBOX_URL=http://user:pw@h:1",
                "must not carry a user",
            ),
            ("OPEN_SANDBOX_URL=http://:pw@h:1", "must not carry a user"),
        ];
        for (word, expected) in cases {
            let err = one(word).unwrap_err();
            assert!(err.contains(expected), "{word}: {err}");
        }
    }

    #[test]
    fn the_timeout_is_a_number_of_at_least_the_minimum() {
        assert!(one("ATHENA_SANDBOX_TIMEOUT_SECS=1800").is_ok());
        let low = format!("ATHENA_SANDBOX_TIMEOUT_SECS={}", MIN_TIMEOUT_SECS - 1);
        assert!(one(&low).unwrap_err().contains("at least 60"));
        for word in [
            "ATHENA_SANDBOX_TIMEOUT_SECS=ten",
            "ATHENA_SANDBOX_TIMEOUT_SECS=-5",
            "ATHENA_SANDBOX_TIMEOUT_SECS=99999999999999999999999",
        ] {
            let err = one(word).unwrap_err();
            assert!(err.contains("must be a number"), "{word}: {err}");
        }
    }
}
