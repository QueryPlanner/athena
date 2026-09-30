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
