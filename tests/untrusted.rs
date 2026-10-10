//! The shared helper for text the model must treat as data (`untrusted`):
//! the nonces that mark it and the fitting into one tool result.

use athena::policy::MAX_RESULT_BYTES;
use athena::untrusted::{NOTICE_BYTES, fit, nonce};

#[test]
fn a_nonce_is_32_hex_digits_and_differs_per_call() {
    let first = nonce();
    let second = nonce();
    assert_eq!(first.len(), 32);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
    assert_ne!(first, second);
}

#[test]
fn text_that_fits_is_kept_whole() {
    assert_eq!(fit("head ", "middle", " tail"), "head middle tail");
}

#[test]
fn text_that_does_not_fit_is_cut_on_a_character_boundary_with_a_notice() {
    let head = "HEAD\n";
    let tail = "\n<<<END>>>";
    // Two-byte characters, so the cut must not split one.
    let text = "é".repeat(MAX_RESULT_BYTES);
    let out = fit(head, &text, tail);
    assert!(out.len() <= MAX_RESULT_BYTES, "{}", out.len());
    assert!(out.starts_with(head));
    assert!(out.ends_with(tail), "the tail is always kept");
    let notice = out
        .lines()
        .find(|l| l.contains("bytes of results left out"))
        .expect("a notice says what was left out");
    assert!(
        notice.starts_with('[') && notice.ends_with("bytes of results left out]"),
        "{notice}"
    );
    assert!(notice.len() <= NOTICE_BYTES);
}
