//! Text the model must treat as data, not instructions: web pages
//! (`search`) and the user's health records (`health::data`).
//!
//! Such text is wrapped in markers carrying a fresh nonce, which a page or a
//! record cannot forge because it is chosen after the text is read, and the
//! result says to treat what is inside as data.

use crate::policy::MAX_RESULT_BYTES;
use crate::sandbox::stream::split_at_boundary;

/// Room kept for the notice that says how much was left out.
pub const NOTICE_BYTES: usize = 128;

/// A fresh nonce for one result's markers.
pub fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// `head`, `text` and `tail` in at most [`MAX_RESULT_BYTES`]: `text` is cut
/// when it does not fit, and a notice says how much was left out.
pub fn fit(head: &str, text: &str, tail: &str) -> String {
    if head.len() + text.len() + tail.len() <= MAX_RESULT_BYTES {
        return format!("{head}{text}{tail}");
    }
    let room = MAX_RESULT_BYTES - head.len() - tail.len() - NOTICE_BYTES;
    let (kept, omitted) = split_at_boundary(text, room);
    let notice = format!("\n[{omitted} bytes of results left out]\n");
    debug_assert!(notice.len() <= NOTICE_BYTES);
    format!("{head}{kept}{notice}{tail}")
}
