//! Signing in to websites through a session's browser, by the user.
//!
//! The agent opens a site and hands the user a link
//! ([`Sandboxes::login_link`]). The page behind it (`http::viewer`) shows
//! the session's browser as screenshots and passes the user's taps and
//! typing to it, so the user signs in themselves and the model never sees a
//! password. Done saves agent-browser's state, cookies and local storage,
//! for the user ([`Sandboxes::save_login`]); every new sandbox of theirs
//! loads it ([`Sandboxes::restore_login`]), so sign-ins outlive sandboxes.
//!
//! Not yet secured: the link's token is the only check, and the saved
//! state is stored unencrypted and loaded where the agent's shell can read
//! it. README "Known limits" lists what that allows.

use super::client::Execd;
use super::stream::Output;
use super::tools::cli_command;
use super::{Error, Sandboxes};
use crate::store::now_millis;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

/// Where the saved state is put in a sandbox, and saved from.
pub const STATE_PATH: &str = "/tmp/athena-browser-state.json";
/// Where the viewer's screenshots are taken, one at a time per session.
pub const SCREEN_PATH: &str = "/tmp/athena-viewer.png";
/// How long a sign-in link opens its session's browser.
pub const LINK_TTL: Duration = Duration::from_secs(3600);
/// The largest state kept. A few sites' cookies are kilobytes; local
/// storage can be more.
pub const MAX_STATE_BYTES: usize = 5 * 1024 * 1024;
/// The largest screenshot the viewer is sent.
pub const MAX_SCREEN_BYTES: usize = 10 * 1024 * 1024;
/// How long one browser command may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// What a sign-in link opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub session_id: String,
    /// Where the browser starts when it shows nothing.
    pub url: String,
}

/// What a control on the page is for, from its role and accessible name,
/// so the sign-in page can offer the right field: a phone fills a
/// `username`, `current-password` or `one-time-code` field itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Username,
    Password,
    Code,
    Text,
    Button,
}

/// A field or button on the page, shown on the sign-in page as a real
/// control. `reference` is agent-browser's ref without the `@`, like `e4`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Control {
    #[serde(rename = "ref")]
    pub reference: String,
    pub name: String,
    pub kind: ControlKind,
}

/// `kind` for an element with this accessibility role and name, or `None`
/// for elements the sign-in page does not offer (links, headings...).
fn kind(role: &str, name: &str) -> Option<ControlKind> {
    let name = name.to_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| name.contains(w));
    match role {
        "button" => Some(ControlKind::Button),
        "textbox" | "searchbox" | "spinbutton" | "combobox" => {
            Some(if says(&["password", "passcode", "passphrase"]) {
                ControlKind::Password
            } else if says(&["code", "otp", "verification", "one-time", "2fa", "pin"]) {
                ControlKind::Code
            } else if says(&["email", "e-mail", "user", "login", "phone", "account"]) {
                ControlKind::Username
            } else {
                ControlKind::Text
            })
        }
        _ => None,
    }
}

/// The controls in `snapshot -i --json` output, in page order: the order
/// of their refs in the snapshot text, named and typed from its `refs`.
fn controls(stdout: &str) -> Result<Vec<Control>, Error> {
    let unreadable = |why: &str| Error::Protocol(format!("agent-browser snapshot: {why}"));
    let value: Value =
        serde_json::from_str(stdout.trim()).map_err(|e| unreadable(&e.to_string()))?;
    let data = &value["data"];
    let (Some(text), Some(refs)) = (data["snapshot"].as_str(), data["refs"].as_object()) else {
        return Err(unreadable("no snapshot or refs"));
    };
    let mut found = Vec::new();
    for line in text.lines() {
        let Some((_, rest)) = line.split_once("ref=") else {
            continue;
        };
        let reference: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let element = refs.get(&reference).unwrap_or(&Value::Null);
        let (role, name) = (element["role"].as_str(), element["name"].as_str());
        if let (Some(role), Some(name)) = (role, name)
            && let Some(kind) = kind(role, name)
        {
            let name = name.to_string();
            found.push(Control {
                reference,
                name,
                kind,
            });
        }
    }
    Ok(found)
}

/// `steps`, each an agent-browser argument list for `session`'s browser,
/// as one command line that stops at the first failure.
fn chain(session: &str, steps: &[&[&str]]) -> Result<String, Error> {
    let lines: Result<Vec<String>, Error> = steps
        .iter()
        .map(|args| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            cli_command(session, &args)
        })
        .collect();
    Ok(lines?.join(" && "))
}

/// `output`, or what went wrong running `what`.
fn succeeded(what: &str, output: Output) -> Result<Output, Error> {
    match &output.error {
        Some(error) => Err(Error::Failed(format!("{what}: {error}"))),
        None => Ok(output),
    }
}

/// Whether `get url` said the browser shows no page.
fn blank(output: &Output) -> bool {
    output.stdout.lines().any(|l| l.trim() == "about:blank")
}

impl Sandboxes {
    /// Load the session owner's saved state into a sandbox just created
    /// for the session. Best effort: a sandbox that cannot load it is still
    /// used, only signed out.
    pub(super) async fn restore_login(&self, session_id: &str, execd: &Execd) {
        if let Err(e) = self.load_state(session_id, execd).await {
            tracing::warn!(session = session_id, "saved sign-ins not restored: {e}");
        }
    }

    /// Load the owner's saved state into the session's browser, if they
    /// have one. The browser restarts with it, on a blank page.
    async fn load_state(&self, session_id: &str, execd: &Execd) -> Result<(), Error> {
        let id = session_id.to_string();
        let state = self.stored(move |s| s.browser_state(&id)).await?;
        let Some(state) = state else {
            return Ok(());
        };
        execd.upload(STATE_PATH, state).await?;
        let load = chain(session_id, &[&["state", "load", STATE_PATH]])?;
        succeeded("state load", execd.command(&load, TIMEOUT).await?)?;
        Ok(())
    }

    /// A link that opens the session's browser at `url` for the user to
    /// sign in. The latest saved state is loaded first, so saving after
    /// this sign-in keeps the sign-ins other sessions saved meanwhile.
    pub async fn login_link(&self, session_id: &str, url: &str) -> Result<String, Error> {
        let lease = self.lease(session_id).await?;
        // A new sandbox has just loaded it.
        if !lease.fresh {
            self.load_state(session_id, &lease.execd).await?;
        }
        let open = chain(session_id, &[&["open", url]])?;
        succeeded("open", lease.execd.command(&open, TIMEOUT).await?)?;
        drop(lease);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let expires_at = now_millis() + LINK_TTL.as_millis() as i64;
        let (id, start, stored) = (session_id.to_string(), url.to_string(), token.clone());
        self.stored(move |s| s.insert_browser_link(&stored, &id, &start, expires_at))
            .await?;
        let mut link = self.config.viewer_url.clone();
        // Cannot fail: the viewer URL is checked to be http(s).
        link.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(["browser", &token]);
        Ok(link.into())
    }

    /// What a sign-in link opens, if the token names one that has not
    /// expired.
    pub async fn linked(&self, token: &str) -> Result<Option<Link>, Error> {
        let token = token.to_string();
        let found = self
            .stored(move |s| s.browser_link(&token, now_millis()))
            .await?;
        Ok(found.map(|(session_id, url)| Link { session_id, url }))
    }

    /// Open the link's start page if the browser shows nothing: its sandbox
    /// expired since the link was made, say.
    pub async fn resume(&self, link: &Link) -> Result<(), Error> {
        let lease = self.lease(&link.session_id).await?;
        let get = chain(&link.session_id, &[&["get", "url"]])?;
        let shown = succeeded("get url", lease.execd.command(&get, TIMEOUT).await?)?;
        if blank(&shown) {
            let open = chain(&link.session_id, &[&["open", &link.url]])?;
            succeeded("open", lease.execd.command(&open, TIMEOUT).await?)?;
        }
        Ok(())
    }

    /// A PNG of what the session's browser shows. One lease for taking and
    /// reading it, so two viewers of the session cannot swap frames, and an
    /// old frame is removed first so a failed screenshot is never mistaken
    /// for a new one.
    pub async fn screen(&self, session_id: &str) -> Result<Vec<u8>, Error> {
        let lease = self.lease(session_id).await?;
        let take = format!(
            "rm -f {SCREEN_PATH} && {}",
            chain(session_id, &[&["screenshot", SCREEN_PATH]])?
        );
        succeeded("screenshot", lease.execd.command(&take, TIMEOUT).await?)?;
        match lease.execd.download(SCREEN_PATH, MAX_SCREEN_BYTES).await? {
            (_, true) => Err(Error::Failed(format!(
                "the screenshot is over {MAX_SCREEN_BYTES} bytes"
            ))),
            (png, false) => Ok(png),
        }
    }

    /// Run agent-browser commands on the session's browser, in order,
    /// stopping at the first that fails.
    pub async fn browser(&self, session_id: &str, steps: &[&[&str]]) -> Result<(), Error> {
        let line = chain(session_id, steps)?;
        let lease = self.lease(session_id).await?;
        succeeded("agent-browser", lease.execd.command(&line, TIMEOUT).await?)?;
        Ok(())
    }

    /// The fields and buttons on the page the session's browser shows.
    pub async fn controls(&self, session_id: &str) -> Result<Vec<Control>, Error> {
        let lease = self.lease(session_id).await?;
        let look = chain(session_id, &[&["snapshot", "-i", "--json"]])?;
        let output = succeeded("snapshot", lease.execd.command(&look, TIMEOUT).await?)?;
        controls(&output.stdout)
    }

    /// Fill fields, then click a button, as one command: `fills` are
    /// (ref, text) pairs, refs without the `@`.
    pub async fn submit(
        &self,
        session_id: &str,
        fills: &[(String, String)],
        click: Option<&str>,
    ) -> Result<(), Error> {
        let refs: Vec<String> = fills.iter().map(|(r, _)| format!("@{r}")).collect();
        let mut steps: Vec<Vec<&str>> = fills
            .iter()
            .zip(&refs)
            .map(|((_, text), at)| vec!["fill", at.as_str(), text.as_str()])
            .collect();
        let button = click.map(|r| format!("@{r}"));
        if let Some(button) = &button {
            steps.push(vec!["click", button]);
        }
        let steps: Vec<&[&str]> = steps.iter().map(Vec::as_slice).collect();
        self.browser(session_id, &steps).await
    }

    /// Save the session's browser state as its owner's, for every sandbox
    /// they get from now on. Returns its size in bytes. Refused when the
    /// sandbox had to be replaced: its browser holds only the old state,
    /// and saving that would report a sign-in that was lost.
    pub async fn save_login(&self, session_id: &str) -> Result<usize, Error> {
        let lease = self.lease(session_id).await?;
        if lease.fresh {
            return Err(Error::Failed(
                "the browser was restarted (its sandbox had expired), so the sign-in \
                 was lost; ask Athena for a new link and sign in again"
                    .into(),
            ));
        }
        let save = chain(session_id, &[&["state", "save", STATE_PATH]])?;
        succeeded("state save", lease.execd.command(&save, TIMEOUT).await?)?;
        let (state, longer) = lease.execd.download(STATE_PATH, MAX_STATE_BYTES).await?;
        drop(lease);
        if longer {
            return Err(Error::Failed(format!(
                "the browser state is over {MAX_STATE_BYTES} bytes; not saved"
            )));
        }
        let (id, size) = (session_id.to_string(), state.len());
        self.stored(move |s| s.save_browser_state(&id, &state, now_millis()))
            .await?;
        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_chained_so_the_first_failure_stops_the_rest() {
        assert_eq!(
            chain("s-1", &[&["mouse", "move", "1", "2"], &["mouse", "down"]]).unwrap(),
            "'agent-browser' '--session' 's-1' '--content-boundaries' 'mouse' 'move' '1' '2' \
             && 'agent-browser' '--session' 's-1' '--content-boundaries' 'mouse' 'down'"
        );
        assert!(chain("s", &[&["keyboard", "type", "nul\0"]]).is_err());
    }

    #[test]
    fn a_failed_command_is_an_error_naming_it() {
        let failed = Output {
            error: Some("exit status 1".into()),
            ..Output::default()
        };
        let err = succeeded("state save", failed).unwrap_err();
        assert_eq!(err.to_string(), "sandbox: state save: exit status 1");
        assert!(succeeded("x", Output::default()).is_ok());
    }

    const SNAPSHOT: &str = r#"{"_boundary":{"nonce":"n","origin":"https://x.example/login"},
        "data":{"refs":{"e1":{"name":"Login","role":"heading"},"e2":{"name":"Password","role":"textbox"},
        "e3":{"name":"Sign in","role":"button"},"e4":{"name":"User Email","role":"textbox"},
        "e5":{"name":"Enter the 6-digit code","role":"textbox"},"e6":{"name":"Search","role":"searchbox"},
        "e7":{"name":"Help","role":"link"}},
        "snapshot":"- heading \"Login\" [level=1, ref=e1]\n- textbox \"User Email\" [ref=e4]\n- textbox \"Password\" [ref=e2]\n- textbox \"Enter the 6-digit code\" [ref=e5]\n- searchbox \"Search\" [ref=e6]\n- link \"Help\" [ref=e7]\n- button \"Sign in\" [ref=e3]\n- text: no ref here"},
        "error":null,"success":true}"#;

    #[test]
    fn controls_come_in_page_order_with_what_each_is_for() {
        let found = controls(SNAPSHOT).unwrap();
        let summary: Vec<(&str, &str, ControlKind)> = found
            .iter()
            .map(|c| (c.reference.as_str(), c.name.as_str(), c.kind))
            .collect();
        assert_eq!(
            summary,
            [
                ("e4", "User Email", ControlKind::Username),
                ("e2", "Password", ControlKind::Password),
                ("e5", "Enter the 6-digit code", ControlKind::Code),
                ("e6", "Search", ControlKind::Text),
                ("e3", "Sign in", ControlKind::Button),
            ]
        );
        assert_eq!(
            serde_json::to_value(&found[1]).unwrap(),
            serde_json::json!({"ref": "e2", "name": "Password", "kind": "password"})
        );
    }

    #[test]
    fn an_empty_page_has_no_controls_and_other_output_is_an_error() {
        let blank = r#"{"data":{"refs":{},"snapshot":"(no interactive elements)"}}"#;
        assert_eq!(controls(blank).unwrap(), []);
        for bad in ["not json", r#"{"data":{}}"#, r#"{"error":"no browser"}"#] {
            assert!(matches!(controls(bad), Err(Error::Protocol(_))), "{bad}");
        }
        // A ref the snapshot names but `refs` lacks is skipped.
        let odd = r#"{"data":{"refs":{},"snapshot":"- textbox \"A\" [ref=e9]"}}"#;
        assert_eq!(controls(odd).unwrap(), []);
    }

    #[test]
    fn kinds_follow_the_role_then_the_name() {
        assert_eq!(
            kind("combobox", "Phone or email"),
            Some(ControlKind::Username)
        );
        assert_eq!(kind("spinbutton", "PIN"), Some(ControlKind::Code));
        assert_eq!(kind("textbox", "Passcode"), Some(ControlKind::Password));
        assert_eq!(kind("heading", "Password"), None);
    }

    #[test]
    fn only_about_blank_is_a_blank_page() {
        let said = |stdout: &str| Output {
            stdout: stdout.into(),
            ..Output::default()
        };
        assert!(blank(&said(
            "[agent-browser] launched browser\nabout:blank\n"
        )));
        assert!(!blank(&said("https://example.com/\n")));
        assert!(!blank(&said("")));
    }
}
