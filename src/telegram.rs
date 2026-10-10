//! The Telegram transport: `athena telegram`, a long-polling bot over
//! [`Service`].
//!
//! Most of this file is plain logic that never touches Telegram: parsing a
//! message into a command or a prompt, choosing the session, splitting a long
//! reply, mapping errors to replies. It talks to a chat through the [`Chat`]
//! trait, so the unit tests below drive it with a recorder and the mock model.
//! The teloxide glue at the bottom is thin: it turns an update into an
//! [`Incoming`] and implements [`Chat`] with Bot API calls. `tests/telegram.rs`
//! runs that glue against a fake Bot API server.
//!
//! Decisions (the README explains them for users):
//!
//! - Private chats only. In a group every member would see one member's
//!   conversation, so group messages are ignored and logged.
//! - A user is `("telegram", <Telegram user id>)`.
//! - Each user talks in one session at a time: the one they last picked with
//!   `/new` or `/switch`, stored in `selected_sessions` so it survives a
//!   restart, or `default` if they never picked one.
//! - One turn per user at a time. A message that arrives while that user's
//!   turn is running is answered with [`BUSY`] and dropped, rather than
//!   queued: the bot is open to anyone, and a queue would let one user line
//!   up paid model calls and fill teloxide's per-chat worker queue, which
//!   stalls every chat.
//! - Message and reply text is never logged; user ids and errors are.
//! - A photo or a file is a prompt, its caption the text. It is downloaded
//!   (Telegram lets bots download up to [`DOWNLOAD_LIMIT`]), put in the
//!   session's sandbox when there is one, and a photo is shown to the model
//!   as an image. Albums arrive as one message per item; the bot collects
//!   them until none has come for [`ALBUM_WAIT`], then runs one turn.
//! - Files the turn's tools send (`send_photo`, `send_file`) follow the
//!   reply. A photo Telegram refuses is sent again as a file.
//! - A voice note or an audio file is transcribed by Cloudflare Workers AI
//!   ([`voice`]) when `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` are
//!   set, and otherwise answered with [`NOT_TEXT`]. Its caption, then the
//!   transcript, is the prompt: an ordinary text turn, the user's one turn
//!   claimed while it is transcribed. The transcript is not echoed back; the
//!   reply shows what was understood, and the transcript is in the session.
//!   The audio is never stored, and one note is transcribed at a time.

pub mod brief;
pub mod health;
pub mod jobs;
pub mod render;
pub mod voice;

use crate::agent;
use crate::health::Health;
use crate::media::{self, Attachment, File, Kind, Outbox};
use crate::runner::{Request, Run};
use crate::sandbox::Sandboxes;
use crate::service::{self, Service, Session, User};
use crate::shutdown;
use crate::store::{self, Store};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use render::Chunk;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use teloxide::dispatching::{DefaultKey, Dispatcher, ShutdownToken, UpdateFilterExt};
use teloxide::net::Download;
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::{Requester, Update};
use teloxide::requests::HasPayload;
use teloxide::types::{BotCommand, ChatAction, ChatId, FileId, InputFile, Message, MessageEntity};
use teloxide::update_listeners::Polling;
use teloxide::{ApiError, Bot, RequestError, dptree};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use voice::{VOICE_LIMIT, VOICE_SECONDS, Whisper};

/// The transport name every Telegram user is stored under.
pub const TRANSPORT: &str = "telegram";

/// The session a user talks in until they pick another.
pub const DEFAULT_SESSION: &str = "default";

/// Longest message Telegram accepts. The Bot API documents `sendMessage`
/// text as "1-4096 characters after entities parsing" and measures entities
/// in UTF-16 code units. [`split`] counts UTF-16 code units, never fewer than
/// characters, so a chunk fits under either reading.
pub const MESSAGE_LIMIT: usize = 4096;

/// Most messages one reply is sent as. Past this the reply is cut short
/// with a note; the whole reply is still in the session's transcript.
pub const MAX_CHUNKS: usize = 8;

/// How often the typing indicator is renewed. Telegram shows it for five
/// seconds or until the bot's next message.
pub const TYPING_EVERY: Duration = Duration::from_secs(4);

/// How long one `getUpdates` long poll waits for news. Below reqwest's 17 s
/// client timeout, as teloxide requires.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Tries per message when Telegram answers 429 Too Many Requests.
const SEND_ATTEMPTS: u32 = 3;

/// How long one Bot API call may take, uploads and downloads included.
/// teloxide's default, 17 s, is too short for a 50 MB file on a slow link;
/// it must stay longer than [`POLL_TIMEOUT`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The largest file a bot can download from Telegram (`getFile`).
pub const DOWNLOAD_LIMIT: usize = 20 * 1024 * 1024;

/// Files downloaded at once, across every user.
const DOWNLOADS: usize = 4;

/// Voice notes downloaded and transcribed at once, across every user. One,
/// so a burst of notes cannot hold many copies of audio and base64 at once.
const VOICES: usize = 1;

/// How long an album is collected after its latest item arrived. Telegram
/// sends the items of an album as separate messages in quick succession and
/// never says how many there are.
pub const ALBUM_WAIT: Duration = Duration::from_secs(2);

/// Items one album can hold, as Telegram allows.
pub const ALBUM_LIMIT: usize = 10;

pub const BUSY: &str =
    "Still working on your last message. Send this one again once I have replied.";
pub const NOT_TEXT: &str = "I read text, photos and files, not this kind of message.";
/// [`NOT_TEXT`] when voice notes are transcribed.
pub const NOT_TEXT_OR_VOICE: &str =
    "I read text, voice notes, photos and files, not this kind of message.";
/// The reply to a voice note that could not be downloaded or transcribed.
pub const NOT_HEARD: &str =
    "I could not transcribe that voice note. Try again, or send it as text.";
pub const SWITCH_USAGE: &str = "Usage: /switch NAME. /sessions lists your sessions.";
pub const LINK_USAGE: &str = "Usage: /link API_USER_ID. Choose a never-used API user ID.";
pub const FAILED: &str = "Something went wrong on my side. Try again in a moment.";

/// The command menu: name, description. Registered with Telegram at startup
/// (`setMyCommands`); names are 1-32 lowercase letters, digits or `_`,
/// descriptions 3-256 characters.
pub const COMMANDS: &[(&str, &str)] = &[
    (
        "new",
        "Start a new session: /new NAME, or /new to have one named",
    ),
    ("sessions", "List your sessions; * marks the current one"),
    ("switch", "Talk in another session: /switch NAME"),
    ("usage", "Tokens used per session"),
    (
        "link",
        "Share your sessions with the API: /link API_USER_ID",
    ),
    (
        "connect_health",
        "Connect Google Health (sleep, activity, heart rate)",
    ),
    (
        "disconnect_health",
        "Disconnect Google Health and delete its token",
    ),
    ("help", "What this bot is and its commands"),
];

/// Receives problems worth an operator's attention. Never message text.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// How the binary logs: a warning through `tracing`, so to stderr and,
/// when configured, to OpenTelemetry.
pub fn log_warning(message: &str) {
    tracing::warn!(transport = TRANSPORT, "{message}");
}

// ---------------- configuration ----------------

/// Whether `args` ask for the bot: `athena telegram`, and nothing after it.
pub fn requested(args: &[String]) -> Result<bool> {
    match args {
        [first, rest @ ..] if first == "telegram" => {
            if !rest.is_empty() {
                bail!("`athena telegram` takes no arguments; it reads TELEGRAM_BOT_TOKEN");
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Where the bot connects. Deliberately not `Debug`: it holds the token.
pub struct Config {
    token: String,
    api_url: Option<url::Url>,
}

impl Config {
    /// `TELEGRAM_BOT_TOKEN`, required. `TELEGRAM_API_URL`, optional: another
    /// Bot API server, such as the fake one the tests run.
    pub fn from_env() -> Result<Self> {
        config(
            std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            std::env::var("TELEGRAM_API_URL").ok(),
        )
    }

    pub fn bot(&self) -> Bot {
        let client = teloxide::net::default_reqwest_settings()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("an HTTP client with rustls builds");
        let bot = Bot::with_client(&self.token, client);
        match &self.api_url {
            Some(url) => bot.set_api_url(url.clone()),
            None => bot,
        }
    }
}

fn config(token: Option<String>, url: Option<String>) -> Result<Config> {
    let token = token.filter(|t| !t.trim().is_empty()).context(
        "TELEGRAM_BOT_TOKEN is not set; create a bot with @BotFather and export its token",
    )?;
    let api_url = url.as_deref().map(api_url).transpose()?;
    Ok(Config { token, api_url })
}

/// An http(s) base URL without a trailing slash. teloxide appends
/// `/bot<token>/<method>` to it, and panics on a URL that cannot be a base
/// (`localhost:8081` parses as scheme `localhost`).
fn api_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw.trim_end_matches('/'))
        .with_context(|| format!("TELEGRAM_API_URL `{raw}` is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        bail!("TELEGRAM_API_URL must be an http or https URL, got `{raw}`");
    }
    Ok(url)
}

// ---------------- parsing ----------------

/// What a message asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Input<'a> {
    Start,
    Help,
    /// `/new [NAME]`.
    New(Option<&'a str>),
    Sessions,
    /// `/switch [NAME]`.
    Switch(Option<&'a str>),
    Usage,
    /// `/link [API_USER_ID]`, handled directly, never sent to the model.
    Link(Option<&'a str>),
    /// `/connect_health`, handled directly.
    ConnectHealth,
    /// `/disconnect_health`, handled directly.
    DisconnectHealth,
    /// A well-formed command this bot does not have.
    Unknown(&'a str),
    /// Anything else: a prompt for the model.
    Text(&'a str),
}

/// Split a message into a command and its argument, or a prompt.
///
/// A command is `/` and 1+ ASCII letters, digits or `_`, optionally followed
/// by `@botname` (Telegram adds it in some clients), then the argument: the
/// rest of the message, trimmed. `/usr/bin is a path` is a prompt.
pub fn parse(text: &str) -> Input<'_> {
    let Some(command) = text.strip_prefix('/') else {
        return Input::Text(text);
    };
    let (word, rest) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let word = word.split_once('@').map_or(word, |(name, _bot)| name);
    if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Input::Text(text);
    }
    let arg = Some(rest.trim()).filter(|a| !a.is_empty());
    match word {
        "start" => Input::Start,
        "help" => Input::Help,
        "new" => Input::New(arg),
        "sessions" => Input::Sessions,
        "switch" => Input::Switch(arg),
        "usage" => Input::Usage,
        "link" => Input::Link(arg),
        "connect_health" => Input::ConnectHealth,
        "disconnect_health" => Input::DisconnectHealth,
        _ => Input::Unknown(word),
    }
}

/// The first `chat-N` not among `taken`, counting up from one past its size.
pub fn free_name(taken: &[String]) -> String {
    (taken.len() + 1..)
        .map(|n| format!("chat-{n}"))
        .find(|name| !taken.contains(name))
        .expect("an unbounded range always has a free name")
}

// ---------------- replies ----------------

/// Break `text` into messages of at most `limit` UTF-16 code units.
///
/// Cuts at the last blank line in the second half of the window, else at the
/// last line break there, else at the last whitespace there, else at the
/// last character that fits: never inside a UTF-8 character, though a hard
/// cut can separate the parts of a multi-codepoint emoji. The break or space
/// cut at is dropped. Chunks that are only whitespace, which Telegram
/// refuses, are skipped. `limit` must be at least 2, the size of the widest
/// character.
pub fn split(text: &str, limit: usize) -> Vec<String> {
    split_ranges(text, limit)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect()
}

/// [`split`], as the byte ranges of `text` the messages are made of. The
/// renderer cuts the entities of a formatted reply at the same places.
fn split_ranges(text: &str, limit: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while utf16_len(&text[at..]) > limit {
        let rest = &text[at..];
        let fits = fit(rest, limit);
        let (end, next) = break_at(&rest[..fits], fits / 2).unwrap_or((fits, fits));
        keep(&mut ranges, text, at..at + end);
        at += next;
    }
    keep(&mut ranges, text, at..text.len());
    ranges
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The byte length of the longest prefix of `s` within `limit` UTF-16 units,
/// and at least its first character.
fn fit(s: &str, limit: usize) -> usize {
    let mut units = 0;
    let end = s
        .char_indices()
        .find(|&(_, c)| {
            units += c.len_utf16();
            units > limit
        })
        .map_or(s.len(), |(i, _)| i);
    end.max(s.chars().next().map_or(0, char::len_utf8))
}

/// Where to cut `window`: (end of this chunk, start of the next), at a blank
/// line, a line break or whitespace no earlier than byte `min`, in that
/// order of preference.
fn break_at(window: &str, min: usize) -> Option<(usize, usize)> {
    let usable = |i: usize| i > 0 && i >= min;
    let blank = window
        .rfind("\n\n")
        .filter(|&i| usable(i))
        .map(|i| (i, i + 2));
    let newline = window
        .rfind('\n')
        .filter(|&i| usable(i))
        .map(|i| (i, i + 1));
    blank.or(newline).or_else(|| {
        window
            .char_indices()
            .rev()
            .find(|&(i, c)| c.is_whitespace() && usable(i))
            .map(|(i, c)| (i, i + c.len_utf8()))
    })
}

fn keep(ranges: &mut Vec<Range<usize>>, text: &str, range: Range<usize>) {
    if !text[range.clone()].trim().is_empty() {
        ranges.push(range);
    }
}

/// The messages a reply is sent as: the model's Markdown rendered and split
/// to fit, at most [`MAX_CHUNKS`].
pub fn chunks(reply: &str) -> Vec<Chunk> {
    let mut parts = render::render(reply);
    if parts.is_empty() {
        return vec![Chunk::plain("(The model sent an empty reply.)")];
    }
    if parts.len() > MAX_CHUNKS {
        let dropped = parts.len() - (MAX_CHUNKS - 1);
        parts.truncate(MAX_CHUNKS - 1);
        parts.push(Chunk::plain(format!(
            "(Reply cut short: {dropped} more messages not sent. The whole reply is saved in this session.)"
        )));
    }
    parts
}

/// What a user is told when a service call fails. The error itself goes to
/// the log: it can carry provider detail the user should not see.
pub fn reply_for(error: &service::Error) -> String {
    match error {
        service::Error::NotFound => "That session no longer exists. /sessions lists yours.".into(),
        service::Error::AlreadyExists(name) => {
            format!("You already have a session named `{name}`. /switch {name} to use it.")
        }
        service::Error::Invalid(why) => why.clone(),
        service::Error::Conflict(_) => "Another process wrote to this session while I was \
             answering, so my reply was not saved. Send your message again."
            .into(),
        service::Error::Model(_) => "The model failed to answer. Try again in a moment.".into(),
        service::Error::Storage(_) => FAILED.into(),
    }
}

fn command_list() -> String {
    COMMANDS
        .iter()
        .map(|(name, what)| format!("/{name} - {what}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn help(current: &str) -> String {
    format!(
        "I am an AI assistant that remembers our conversation. Send me a message \
         to talk; I keep separate conversations in sessions.\n\n\
         You are in session `{current}`.\n\n{}",
        command_list()
    )
}

// ---------------- the bot's logic ----------------

/// One chat's side effects. The teloxide implementation calls the Bot API;
/// tests record.
pub trait Chat: Clone + Send + Sync + 'static {
    /// Show "typing..." for a few seconds.
    fn typing(&self) -> impl Future<Output = Result<()>> + Send;
    /// Send one message, at most [`MESSAGE_LIMIT`] long.
    fn say(&self, text: &str) -> impl Future<Output = Result<()>> + Send;
    /// Send one message formatted by `entities`. Telegram answering that it
    /// cannot apply them is a [`Refused`] error.
    fn say_formatted(
        &self,
        text: &str,
        entities: &[MessageEntity],
    ) -> impl Future<Output = Result<()>> + Send;
    /// Download a file the user sent; fail past `limit` bytes.
    fn download(&self, id: &str, limit: usize) -> impl Future<Output = Result<Vec<u8>>> + Send;
    /// Send the user a file, as a photo or a document.
    fn send_file(&self, attachment: &Attachment) -> impl Future<Output = Result<()>> + Send;
}

/// A message as the logic needs it, whatever transport library delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub chat_id: i64,
    pub private: bool,
    /// The sender's Telegram user id; absent for channel posts.
    pub user_id: Option<u64>,
    /// The text, or a photo's or file's caption. Absent for stickers and
    /// other messages without one.
    pub text: Option<String>,
    /// The photo or file the message carries: Telegram sends at most one.
    pub files: Vec<IncomingFile>,
    /// The album (Telegram's `media_group_id`) the message belongs to.
    pub album: Option<String>,
    /// The voice note or audio file the message carries.
    pub voice: Option<IncomingVoice>,
}

/// A voice note or audio file in a message, not yet downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingVoice {
    /// Telegram's `file_id`, for `getFile`.
    pub id: String,
    /// As Telegram reports it; 0 if it does not.
    pub size: u64,
    /// As the sender's client reports it.
    pub seconds: u32,
}

/// Why `voice` is not transcribed, if it is not, known before it is
/// downloaded: it is larger or longer than the limits.
pub fn unheard(voice: &IncomingVoice) -> Option<String> {
    const MB: u64 = 1024 * 1024;
    if voice.size > VOICE_LIMIT as u64 {
        return Some(format!(
            "That voice note is {} MB; I can transcribe up to {} MB.",
            voice.size.div_ceil(MB),
            VOICE_LIMIT as u64 / MB
        ));
    }
    if voice.seconds > VOICE_SECONDS {
        return Some(format!(
            "That voice note is {} minutes long; I can transcribe up to {} minutes.",
            voice.seconds.div_ceil(60),
            VOICE_SECONDS / 60
        ));
    }
    None
}

/// The prompt a voice note makes: its caption, if any, then the transcript.
pub fn spoken(caption: &str, transcript: &str) -> String {
    match caption.trim() {
        "" => transcript.to_string(),
        caption => format!("{caption}\n\n{transcript}"),
    }
}

/// A photo or file in a message, not yet downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingFile {
    /// Telegram's `file_id`, for `getFile`.
    pub id: String,
    /// As the sender named it, not yet made safe.
    pub name: String,
    pub mime: Option<String>,
    /// As Telegram reports it; 0 if it does not.
    pub size: u64,
}

/// The reply to a file too large to download.
pub fn too_big(file: &IncomingFile) -> String {
    format!(
        "`{}` is {} MB; I can only receive files up to {} MB.",
        file.name,
        file.size.div_ceil(1024 * 1024),
        DOWNLOAD_LIMIT / (1024 * 1024)
    )
}

/// The bot: turns messages into service calls and replies.
pub struct Telegram<R> {
    service: Arc<Service>,
    /// The same database as `service`, for the session selection, which the
    /// service does not expose yet.
    store: Store,
    agent: Arc<R>,
    /// Where users' files are put; none without a sandbox server.
    sandboxes: Option<Arc<Sandboxes>>,
    /// Permits for [`DOWNLOADS`].
    downloads: Arc<Semaphore>,
    /// Transcribes voice notes; none without Cloudflare settings.
    whisper: Option<Arc<Whisper>>,
    /// Google Health; none without its settings. The same service the
    /// agent's tools use.
    health: Option<Arc<Health>>,
    /// Permits for [`VOICES`].
    voices: Arc<Semaphore>,
    log: Log,
    typing_every: Duration,
    /// Users with a turn running.
    busy: Arc<Mutex<HashSet<u64>>>,
    /// Albums being collected, by user and album id.
    albums: Mutex<HashMap<(u64, String), Album>>,
    album_wait: Duration,
    /// Turns running or finished and not yet reaped. [`Telegram::finish`]
    /// waits for them.
    turns: Mutex<JoinSet<()>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An album's items so far.
struct Album {
    text: Option<String>,
    files: Vec<IncomingFile>,
    /// When the latest item arrived.
    last: tokio::time::Instant,
}

/// Where a turn's text came from.
enum Origin {
    /// The user typed it, or captioned the files they sent.
    Typed,
    /// The caption of this voice note; its transcript follows.
    Voice(IncomingVoice),
    /// A scheduled task: nobody typed it just now.
    Scheduled,
}

/// A claim on a user's one turn. Released when dropped, panics included.
struct Busy {
    users: Arc<Mutex<HashSet<u64>>>,
    user: u64,
}

impl Busy {
    fn claim(users: &Arc<Mutex<HashSet<u64>>>, user: u64) -> Option<Self> {
        lock(users).insert(user).then(|| Self {
            users: users.clone(),
            user,
        })
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        lock(&self.users).remove(&self.user);
    }
}

impl<R: Run + 'static> Telegram<R> {
    /// `store` must be the store `service` was built on.
    pub fn new(service: Arc<Service>, store: Store, agent: R, log: Log) -> Self {
        Self {
            service,
            store,
            agent: Arc::new(agent),
            sandboxes: None,
            downloads: Arc::new(Semaphore::new(DOWNLOADS)),
            whisper: None,
            health: None,
            voices: Arc::new(Semaphore::new(VOICES)),
            log,
            typing_every: TYPING_EVERY,
            busy: Arc::default(),
            albums: Mutex::default(),
            album_wait: ALBUM_WAIT,
            turns: Mutex::default(),
        }
    }

    /// Put the files users send in these sandboxes: the ones the agent's
    /// tools use.
    pub fn sandboxes(mut self, sandboxes: Option<Arc<Sandboxes>>) -> Self {
        self.sandboxes = sandboxes;
        self
    }

    /// Transcribe voice notes with `whisper`; without one they are not
    /// prompts.
    pub fn voice(mut self, whisper: Option<Arc<Whisper>>) -> Self {
        self.whisper = whisper;
        self
    }

    /// Connect and sync Google Health with `health`; without it the
    /// commands say it is not set up.
    pub fn health(mut self, health: Option<Arc<Health>>) -> Self {
        self.health = health;
        self
    }

    /// Collect an album for this long after its latest item instead of
    /// [`ALBUM_WAIT`].
    pub fn album_wait(mut self, wait: Duration) -> Self {
        self.album_wait = wait;
        self
    }

    /// Renew the typing indicator this often instead of [`TYPING_EVERY`].
    pub fn typing_every(mut self, every: Duration) -> Self {
        self.typing_every = every;
        self
    }

    /// Wait for every turn that has started to reply.
    pub async fn finish(&self) {
        let mut turns = std::mem::take(&mut *lock(&self.turns));
        while turns.join_next().await.is_some() {}
    }

    /// Handle one message in its own task, so a panic anywhere in it is
    /// logged and answered instead of killing teloxide's worker for the chat.
    pub async fn handle_isolated<C: Chat>(self: &Arc<Self>, chat: C, incoming: Incoming) {
        let (app, to) = (self.clone(), chat.clone());
        isolated(
            &chat,
            &self.log,
            async move { app.handle(to, incoming).await },
        )
        .await;
    }

    /// Handle one message. Commands are answered before this returns; a
    /// prompt starts a turn that replies on its own.
    pub async fn handle<C: Chat>(self: &Arc<Self>, chat: C, incoming: Incoming) {
        let Some(user_id) = self.accept(&incoming) else {
            return;
        };
        let reply = match self.respond(&chat, user_id, incoming).await {
            Ok(Some(reply)) => reply,
            Ok(None) => return,
            Err(e) => {
                (self.log)(&format!("telegram user {user_id}: {e}"));
                reply_for(&e)
            }
        };
        say(&chat, &self.log, &reply).await;
    }

    /// The sender, if this is a message the bot answers.
    fn accept(&self, incoming: &Incoming) -> Option<u64> {
        match (incoming.private, incoming.user_id) {
            (true, Some(user)) => Some(user),
            (false, _) => {
                (self.log)(&format!(
                    "ignoring a message in chat {}: only private chats are served",
                    incoming.chat_id
                ));
                None
            }
            (true, None) => {
                (self.log)(&format!(
                    "ignoring a message with no sender in chat {}",
                    incoming.chat_id
                ));
                None
            }
        }
    }

    /// The reply to send now, or `None` if a turn was started.
    async fn respond<C: Chat>(
        self: &Arc<Self>,
        chat: &C,
        user_id: u64,
        incoming: Incoming,
    ) -> Result<Option<String>, service::Error> {
        // First of all, and whatever else the message carries: a pasted
        // Google callback URL holds a one-time code and is not a prompt, a
        // caption or a command.
        let redirect = self.health.as_ref().map(|h| h.redirect());
        let pasted = incoming
            .text
            .as_deref()
            .and_then(|text| crate::health::find_callback(text, redirect));
        if let Some(callback) = pasted {
            return self.paste(chat, user_id, callback).await;
        }
        if let Some(voice) = incoming.voice {
            // Like a file's caption, a voice note's is never a command.
            if self.whisper.is_none() {
                return Ok(Some(NOT_TEXT.into()));
            }
            if let Some(why) = unheard(&voice) {
                return Ok(Some(why));
            }
            let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
            let text = incoming.text.unwrap_or_default();
            return self
                .start_turn(chat, user, user_id, text, Vec::new(), Some(voice))
                .await;
        }
        if !incoming.files.is_empty() {
            // A caption is never a command: the file is what was sent.
            if let Some(big) = incoming
                .files
                .iter()
                .find(|f| f.size > DOWNLOAD_LIMIT as u64)
            {
                return Ok(Some(too_big(big)));
            }
            if let Some(album) = incoming.album {
                self.collect(chat, user_id, album, incoming.text, incoming.files);
                return Ok(None);
            }
            let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
            let text = incoming.text.unwrap_or_default();
            return self
                .start_turn(chat, user, user_id, text, incoming.files, None)
                .await;
        }
        let Some(text) = incoming.text.as_deref() else {
            let reply = match self.whisper {
                Some(_) => NOT_TEXT_OR_VOICE,
                None => NOT_TEXT,
            };
            return Ok(Some(reply.into()));
        };
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let reply = match parse(text) {
            Input::Start | Input::Help => help(&self.current(&user).await?.name),
            Input::New(name) => self.create(&user, name).await?,
            Input::Sessions => self.list(&user).await?,
            Input::Switch(None) => SWITCH_USAGE.into(),
            Input::Switch(Some(name)) => self.switch(&user, name).await?,
            Input::Usage => self.usage(&user).await?,
            Input::Link(None) => LINK_USAGE.into(),
            Input::Link(Some(id)) => {
                self.service.link_http_user(&user, id).await?;
                format!(
                    "Linked. Send `X-Athena-User: {id}` with API requests to access your sessions. Saved browser sign-ins are shared too. This trusts the private network; the header is not authentication."
                )
            }
            Input::ConnectHealth => self.connect_health(&user).await?,
            Input::DisconnectHealth => self.disconnect_health(&user).await?,
            Input::Unknown(cmd) => format!("Unknown command /{cmd}.\n\n{}", command_list()),
            Input::Text(prompt) => {
                return self
                    .start_turn(chat, user, user_id, prompt.into(), Vec::new(), None)
                    .await;
            }
        };
        Ok(Some(reply))
    }

    /// The session `user` is talking in: the one they selected, else `default`.
    async fn current(&self, user: &User) -> Result<Session, service::Error> {
        let owner = user.clone();
        match self.store.call(move |s| s.selected_session(&owner)).await? {
            Some(session) => Ok(session),
            None => self.service.open_session(user, DEFAULT_SESSION).await,
        }
    }

    async fn select(&self, user: &User, session: &Session) -> Result<(), service::Error> {
        let (owner, id) = (user.clone(), session.id.clone());
        let selected = self
            .store
            .call(move |s| s.select_session(&owner, &id))
            .await?;
        selected.then_some(()).ok_or(service::Error::NotFound)
    }

    async fn create(&self, user: &User, name: Option<&str>) -> Result<String, service::Error> {
        let name = match name {
            Some(name) if name.contains(['\n', '\r']) => {
                return Err(service::Error::Invalid(
                    "A session name must fit on one line.".into(),
                ));
            }
            Some(name) => name.to_string(),
            None => {
                let taken: Vec<String> = self
                    .service
                    .sessions(user)
                    .await?
                    .into_iter()
                    .map(|s| s.session.name)
                    .collect();
                free_name(&taken)
            }
        };
        let session = self.service.create_session(user, &name).await?;
        self.select(user, &session).await?;
        Ok(format!(
            "Started session `{}`. Your messages go to it now.",
            session.name
        ))
    }

    async fn switch(&self, user: &User, name: &str) -> Result<String, service::Error> {
        let found = self
            .service
            .sessions(user)
            .await?
            .into_iter()
            .find(|s| s.session.name == name);
        let found = match found {
            Some(found) => Some(found),
            // `default` is every user's home, even before it has a message.
            None if name == DEFAULT_SESSION => Some(service::SessionSummary {
                session: self.service.open_session(user, name).await?,
                messages: 0,
            }),
            None => None,
        };
        let Some(found) = found else {
            return Ok(format!(
                "You have no session named `{name}`. /sessions lists yours; /new {name} creates it."
            ));
        };
        self.select(user, &found.session).await?;
        Ok(format!(
            "Switched to `{}` ({} messages).",
            found.session.name, found.messages
        ))
    }

    async fn list(&self, user: &User) -> Result<String, service::Error> {
        let current = self.current(user).await?;
        let lines: Vec<String> = self
            .service
            .sessions(user)
            .await?
            .into_iter()
            .map(|s| {
                let mark = if s.session.id == current.id { '*' } else { ' ' };
                format!("{mark} {} ({} messages)", s.session.name, s.messages)
            })
            .collect();
        Ok(format!("Your sessions:\n{}", lines.join("\n")))
    }

    async fn usage(&self, user: &User) -> Result<String, service::Error> {
        let rows = self.service.usage(user).await?;
        if rows.is_empty() {
            return Ok("No turns yet.".into());
        }
        let lines: Vec<String> = rows
            .iter()
            .map(|u| {
                format!(
                    "{}: {} turns, {} model calls, {} tokens in ({} cached), {} out",
                    u.name,
                    u.runs,
                    u.model_calls,
                    u.input_tokens,
                    u.cached_input_tokens,
                    u.output_tokens
                )
            })
            .collect();
        Ok(lines.join("\n"))
    }

    /// Add an album's item. The first item starts a task that waits until
    /// no item has arrived for [`Telegram::album_wait`], then runs one turn
    /// for all of them, in that task so [`Telegram::finish`] waits for it.
    fn collect<C: Chat>(
        self: &Arc<Self>,
        chat: &C,
        user_id: u64,
        album: String,
        text: Option<String>,
        files: Vec<IncomingFile>,
    ) {
        let key = (user_id, album);
        let now = tokio::time::Instant::now();
        let mut albums = lock(&self.albums);
        if let Some(open) = albums.get_mut(&key) {
            if open.files.len() + files.len() > ALBUM_LIMIT {
                (self.log)(&format!(
                    "telegram user {user_id}: an album has more than {ALBUM_LIMIT} items; \
                     ignoring the rest"
                ));
            } else {
                open.files.extend(files);
            }
            // Telegram puts an album's caption on one item, not always the first.
            open.text = open.text.take().or(text);
            open.last = now;
            return;
        }
        albums.insert(
            key.clone(),
            Album {
                text,
                files,
                last: now,
            },
        );
        drop(albums);
        let (app, chat) = (self.clone(), chat.clone());
        let mut turns = lock(&self.turns);
        while turns.try_join_next().is_some() {}
        turns.spawn(async move {
            let album = loop {
                let last = lock(&app.albums)[&key].last;
                tokio::time::sleep_until(last + app.album_wait).await;
                let mut albums = lock(&app.albums);
                if albums[&key].last == last {
                    break albums.remove(&key).expect("the album is still collected");
                }
            };
            let text = album.text.unwrap_or_default();
            let reply = match app.claim_turn(user_id).await {
                Ok(Some((busy, user, session))) => {
                    let _busy = busy;
                    app.turn(chat, user, session, text, album.files, Origin::Typed)
                        .await;
                    return;
                }
                Ok(None) => BUSY.into(),
                Err(e) => {
                    (app.log)(&format!("telegram user {user_id}: {e}"));
                    reply_for(&e)
                }
            };
            say(&chat, &app.log, &reply).await;
        });
    }

    /// The user's turn and their current session, or `None` if a turn of
    /// theirs is already running.
    async fn claim_turn(
        &self,
        user_id: u64,
    ) -> Result<Option<(Busy, User, Session)>, service::Error> {
        let Some(busy) = Busy::claim(&self.busy, user_id) else {
            return Ok(None);
        };
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let session = self.current(&user).await?;
        Ok(Some((busy, user, session)))
    }

    /// Start a turn for `text`, `files` and `voice` in the user's current
    /// session, unless one of theirs is already running. Returns what to
    /// reply now, if anything.
    async fn start_turn<C: Chat>(
        self: &Arc<Self>,
        chat: &C,
        user: User,
        user_id: u64,
        text: String,
        files: Vec<IncomingFile>,
        voice: Option<IncomingVoice>,
    ) -> Result<Option<String>, service::Error> {
        let Some(busy) = Busy::claim(&self.busy, user_id) else {
            return Ok(Some(BUSY.into()));
        };
        let session = self.current(&user).await?;
        let (app, chat) = (self.clone(), chat.clone());
        let mut turns = lock(&self.turns);
        while turns.try_join_next().is_some() {}
        turns.spawn(async move {
            let _busy = busy;
            let origin = voice.map_or(Origin::Typed, Origin::Voice);
            app.turn(chat, user, session, text, files, origin).await;
        });
        Ok(None)
    }

    /// Download `file` and put it in `session`'s sandbox. A failure is not
    /// the turn's: the model is told the file is missing and why.
    ///
    /// Only the image shown to the model stays in memory; the bytes move to
    /// the sandbox. A file nothing could use (no sandbox, and not sent as an
    /// image) is not downloaded at all.
    async fn receive<C: Chat>(&self, chat: &C, session: &Session, file: IncomingFile) -> File {
        let unreceived = |why: &str| File {
            name: media::safe_name(&file.name),
            mime: file.mime.clone(),
            size: file.size,
            image: None,
            saved: Err(why.into()),
        };
        let sent_as_image = file
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"));
        if self.sandboxes.is_none() && !sent_as_image {
            return unreceived("this assistant has no sandbox");
        }
        // Cannot fail: the semaphore is never closed.
        let _permit = self
            .downloads
            .acquire()
            .await
            .expect("downloads is never closed");
        let bytes = match chat.download(&file.id, DOWNLOAD_LIMIT).await {
            Ok(bytes) => bytes,
            Err(e) => {
                (self.log)(&format!("downloading a file from Telegram failed: {e:#}"));
                return unreceived("it could not be downloaded from Telegram");
            }
        };
        let mut received = unreceived("this assistant has no sandbox");
        received.size = bytes.len() as u64;
        received.image = media::shown(&bytes, file.mime.as_deref());
        if let Some(sandboxes) = &self.sandboxes {
            received.saved = sandboxes
                .stage(&session.id, &received.name, bytes)
                .await
                .map_err(|e| {
                    (self.log)(&format!("saving a file in the sandbox failed: {e}"));
                    format!("saving it failed: {e}")
                });
        }
        received
    }

    /// The text spoken in `voice`, or `None` after logging why not. One
    /// note at a time is downloaded and transcribed, and its audio is gone
    /// when this returns.
    async fn transcribe<C: Chat>(&self, chat: &C, voice: &IncomingVoice) -> Option<String> {
        let whisper = self.whisper.as_ref()?;
        // Cannot fail: the semaphores are never closed.
        let _voice = self.voices.acquire().await.expect("voices is never closed");
        let download = self
            .downloads
            .acquire()
            .await
            .expect("downloads is never closed");
        let audio = chat.download(&voice.id, VOICE_LIMIT).await;
        drop(download);
        let heard = match audio {
            Ok(audio) => whisper.transcribe(audio).await,
            Err(e) => Err(e.context("downloading it from Telegram failed")),
        };
        heard
            .inspect_err(|e| (self.log)(&format!("transcribing a voice note failed: {e:#}")))
            .ok()
    }

    /// Run one turn and send its reply, then the files its tools sent,
    /// showing "typing..." meanwhile. A voice note is transcribed first, and
    /// without a transcript the turn does not run.
    ///
    /// The model call runs in its own task: a panic in it becomes a reply,
    /// and nothing cancels it once started. Returns why the reply could not
    /// be sent, if it could not: the rest of it is not tried.
    async fn turn<C: Chat>(
        &self,
        chat: C,
        user: User,
        session: Session,
        text: String,
        files: Vec<IncomingFile>,
        origin: Origin,
    ) -> Option<anyhow::Error> {
        typing(&chat, &self.log).await;
        let typing = tokio::spawn(keep_typing(
            chat.clone(),
            self.typing_every,
            self.log.clone(),
        ));
        let text = match &origin {
            Origin::Typed | Origin::Scheduled => text,
            Origin::Voice(voice) => match self.transcribe(&chat, voice).await {
                Some(transcript) => spoken(&text, &transcript),
                None => {
                    typing.abort();
                    let _ = typing.await;
                    say(&chat, &self.log, NOT_HEARD).await;
                    return None;
                }
            },
        };
        let mut received = Vec::with_capacity(files.len());
        for file in files {
            received.push(self.receive(&chat, &session, file).await);
        }
        let outbox = Outbox::default();
        let request = Request {
            text,
            files: received,
            outbox: Some(outbox.clone()),
            scheduled: matches!(origin, Origin::Scheduled),
        };
        let (service, agent) = (self.service.clone(), self.agent.clone());
        let who = user.external_id().to_string();
        let sent =
            tokio::spawn(async move { service.send(&*agent, &user, &session.id, request).await })
                .await;
        typing.abort();
        // Wait for it to stop, so no indicator can land after the reply.
        let _ = typing.await;
        let reply = match sent {
            Ok(Ok(turn)) => turn.reply,
            Ok(Err(e)) => {
                (self.log)(&format!("telegram user {who}: turn failed: {e}"));
                reply_for(&e)
            }
            Err(e) => {
                (self.log)(&format!("telegram user {who}: turn panicked: {e}"));
                FAILED.into()
            }
        };
        let mut unsent = None;
        for chunk in chunks(&reply) {
            if let Err(e) = say_chunk(&chat, &self.log, &chunk).await {
                unsent = Some(e);
                break;
            }
        }
        // Even after a failed turn: the files were made before it failed.
        for attachment in outbox.take() {
            deliver(&chat, &self.log, attachment).await;
        }
        unsent
    }
}

/// Telegram answered a send with an error, rather than the send failing
/// on the way: the file did not arrive, and sending it differently may work.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Telegram refused it: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Send one file the turn's tools queued. Telegram refuses some images as
/// photos (too large, odd proportions); those go again as a file. Any other
/// failure is not retried: a timed-out upload may still have arrived. A file
/// that cannot be sent is logged and mentioned in the chat.
async fn deliver<C: Chat>(chat: &C, log: &Log, attachment: Attachment) {
    let mut sent = chat.send_file(&attachment).await;
    if let (Err(e), Kind::Photo) = (&sent, attachment.kind)
        && e.is::<Refused>()
    {
        log(&format!(
            "sending a photo failed, sending it as a file: {e:#}"
        ));
        let document = Attachment {
            kind: Kind::Document,
            ..attachment.clone()
        };
        sent = chat.send_file(&document).await;
    }
    if let Err(e) = sent {
        log(&format!("sending a file failed: {e:#}"));
        say(
            chat,
            log,
            &format!("(I could not send you `{}`.)", attachment.name),
        )
        .await;
    }
}

/// Run `work` in its own task. If it panics, log it and tell the chat.
async fn isolated<C: Chat>(chat: &C, log: &Log, work: impl Future<Output = ()> + Send + 'static) {
    if let Err(e) = tokio::spawn(work).await {
        log(&format!("handling a message in its own task failed: {e}"));
        say(chat, log, FAILED).await;
    }
}

/// Send one message; log a failure. Returns whether it was sent.
async fn say<C: Chat>(chat: &C, log: &Log, text: &str) -> bool {
    sent(chat, log, text).await.is_ok()
}

/// [`say`], returning why the message was not sent.
async fn sent<C: Chat>(chat: &C, log: &Log, text: &str) -> Result<()> {
    chat.say(text)
        .await
        .inspect_err(|e| log(&format!("sending a message failed: {e:#}")))
}

/// Send one chunk of a reply with its formatting. If Telegram refuses the
/// formatting, the same text is sent without it: a reply must never be lost
/// to a formatting error. Returns why the text was not sent, if it was not.
async fn say_chunk<C: Chat>(chat: &C, log: &Log, chunk: &Chunk) -> Result<()> {
    if chunk.entities.is_empty() {
        return sent(chat, log, &chunk.text).await;
    }
    match chat.say_formatted(&chunk.text, &chunk.entities).await {
        Ok(()) => Ok(()),
        Err(e) if e.is::<Refused>() => {
            log(&format!(
                "sending formatted text failed, sending it as plain text: {e:#}"
            ));
            sent(chat, log, &chunk.text).await
        }
        Err(e) => {
            log(&format!("sending a message failed: {e:#}"));
            Err(e)
        }
    }
}

async fn typing<C: Chat>(chat: &C, log: &Log) {
    if let Err(e) = chat.typing().await {
        log(&format!("sending the typing indicator failed: {e:#}"));
    }
}

/// Renew the typing indicator every `every` until aborted.
async fn keep_typing<C: Chat>(chat: C, every: Duration, log: Log) {
    loop {
        tokio::time::sleep(every).await;
        typing(&chat, &log).await;
    }
}

// ---------------- teloxide glue ----------------

/// One Telegram chat, through the Bot API.
#[derive(Clone)]
struct TelegramChat {
    bot: Bot,
    chat: ChatId,
}

impl Chat for TelegramChat {
    async fn typing(&self) -> Result<()> {
        self.bot
            .send_chat_action(self.chat, ChatAction::Typing)
            .await?;
        Ok(())
    }

    async fn say(&self, text: &str) -> Result<()> {
        retrying(SEND_ATTEMPTS, || self.bot.send_message(self.chat, text)).await?;
        Ok(())
    }

    async fn say_formatted(&self, text: &str, entities: &[MessageEntity]) -> Result<()> {
        retrying(SEND_ATTEMPTS, || {
            self.bot
                .send_message(self.chat, text)
                .entities(entities.to_vec())
        })
        .await
        .map_err(bad_request)?;
        Ok(())
    }

    async fn download(&self, id: &str, limit: usize) -> Result<Vec<u8>> {
        let file = retrying(SEND_ATTEMPTS, || self.bot.get_file(FileId(id.into()))).await?;
        let mut body = self.bot.download_file_stream(&file.path);
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk?);
            if bytes.len() > limit {
                bail!("the file is over {limit} bytes");
            }
        }
        Ok(bytes)
    }

    async fn send_file(&self, attachment: &Attachment) -> Result<()> {
        let file =
            || InputFile::memory(attachment.bytes.clone()).file_name(attachment.name.clone());
        let caption = attachment.caption.clone();
        match attachment.kind {
            Kind::Photo => {
                retrying(SEND_ATTEMPTS, || {
                    let caption = caption.clone();
                    self.bot
                        .send_photo(self.chat, file())
                        .with_payload_mut(|p| p.caption = caption)
                })
                .await
                .map_err(refused)?;
            }
            Kind::Document => {
                retrying(SEND_ATTEMPTS, || {
                    let caption = caption.clone();
                    self.bot
                        .send_document(self.chat, file())
                        .with_payload_mut(|p| p.caption = caption)
                })
                .await
                .map_err(refused)?;
            }
        }
        Ok(())
    }
}

/// A failed send, as [`Refused`] when Telegram answered it with an error.
fn refused(e: RequestError) -> anyhow::Error {
    match e {
        RequestError::Api(api) => Refused(api.to_string()).into(),
        other => other.into(),
    }
}

/// A failed formatted send, as [`Refused`] when Telegram says it cannot
/// parse the entities, or answers a "Bad Request" teloxide has no name for:
/// sending the text again without formatting may work. Any other failure (a
/// block, a timeout, a server error, a chat that does not exist) would fail
/// the same way again.
fn bad_request(e: RequestError) -> anyhow::Error {
    match e {
        RequestError::Api(ApiError::CantParseEntities(why)) => Refused(why).into(),
        RequestError::Api(ApiError::Unknown(why)) if why.starts_with("Bad Request") => {
            Refused(why).into()
        }
        other => other.into(),
    }
}

/// Call `request` until it does not answer 429, waiting as long as Telegram
/// asks, at most `attempts` times.
async fn retrying<T, F, Fut>(attempts: u32, mut request: F) -> Result<T, RequestError>
where
    F: FnMut() -> Fut,
    Fut: std::future::IntoFuture<Output = Result<T, RequestError>>,
{
    let mut left = attempts;
    loop {
        match request().await {
            Err(RequestError::RetryAfter(wait)) if left > 1 => {
                left -= 1;
                tokio::time::sleep(wait.duration()).await;
            }
            result => return result,
        }
    }
}

pub type TelegramDispatcher = Dispatcher<Bot, Infallible, DefaultKey>;

/// The teloxide dispatcher for `app`. Messages go to [`Telegram::handle`];
/// other updates are logged. Stop it with its `shutdown_token()`: `main`
/// does that on SIGINT or SIGTERM, tests do it directly.
pub fn dispatcher<R: Run + 'static>(bot: Bot, app: Arc<Telegram<R>>) -> TelegramDispatcher {
    let log = app.log.clone();
    let handler = Update::filter_message().endpoint(on_message::<R>);
    let builder = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .default_handler(move |update: Arc<Update>| {
            let log = log.clone();
            async move { log(&format!("ignoring update {}: not a message", update.id.0)) }
        });
    builder.build()
}

/// Shut the dispatcher down on the first stop signal from `stop` that
/// arrives while it is polling; `serve` then waits for the turns in flight.
/// This is what teloxide's own Ctrl-C handler does, for SIGTERM too. A
/// signal before polling has started is ignored. The returned task ends
/// once it has shut the dispatcher down.
pub fn stop_on<F: Future<Output = ()> + Send + 'static>(
    token: ShutdownToken,
    stop: impl Fn() -> F + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            stop().await;
            if token.shutdown().is_ok() {
                break;
            }
        }
    })
}

async fn on_message<R: Run + 'static>(
    bot: Bot,
    message: Message,
    app: Arc<Telegram<R>>,
) -> Result<(), Infallible> {
    let files = files(&message);
    let voice = voice_note(&message);
    let incoming = Incoming {
        chat_id: message.chat.id.0,
        private: message.chat.is_private(),
        user_id: message.from.as_ref().map(|user| user.id.0),
        // A caption only counts with the photo, file or voice note it came
        // with: a captioned video is not a prompt, and its caption not a
        // command.
        text: message
            .text()
            .or(message
                .caption()
                .filter(|_| !files.is_empty() || voice.is_some()))
            .map(str::to_string),
        files,
        album: message.media_group_id().map(|id| id.0.clone()),
        voice,
    };
    let chat = TelegramChat {
        bot,
        chat: message.chat.id,
    };
    app.handle_isolated(chat, incoming).await;
    Ok(())
}

/// The photo or file in `message`. Of a photo's sizes, the largest:
/// Telegram lists them smallest first, and sends photos as JPEG.
fn files(message: &Message) -> Vec<IncomingFile> {
    let photo = message
        .photo()
        .and_then(|sizes| sizes.iter().max_by_key(|s| s.width * s.height))
        .map(|size| IncomingFile {
            id: size.file.id.0.clone(),
            name: "photo.jpg".into(),
            mime: Some("image/jpeg".into()),
            size: size.file.size.into(),
        });
    let document = message.document().map(|doc| IncomingFile {
        id: doc.file.id.0.clone(),
        name: doc.file_name.clone().unwrap_or_else(|| "file".into()),
        mime: doc.mime_type.as_ref().map(ToString::to_string),
        size: doc.file.size.into(),
    });
    photo.into_iter().chain(document).collect()
}

/// The voice note or audio file in `message`.
fn voice_note(message: &Message) -> Option<IncomingVoice> {
    let voice = message.voice().map(|v| (&v.file, v.duration));
    let audio = message.audio().map(|a| (&a.file, a.duration));
    voice.or(audio).map(|(file, duration)| IncomingVoice {
        id: file.id.0.clone(),
        size: file.size.into(),
        seconds: duration.seconds(),
    })
}

fn bot_commands() -> Vec<BotCommand> {
    COMMANDS
        .iter()
        .map(|(name, what)| BotCommand::new(*name, *what))
        .collect()
}

/// Register the command menu, poll for updates until shut down, then wait
/// for turns still running. Fails if Telegram refuses the token.
pub async fn serve<R: Run + 'static>(
    dispatcher: &mut TelegramDispatcher,
    bot: Bot,
    app: &Telegram<R>,
) -> Result<()> {
    let log = app.log.clone();
    if let Err(e) = bot.set_my_commands(bot_commands()).await {
        log(&format!("registering the command menu failed: {e}"));
    }
    let listener = Polling::builder(bot)
        .timeout(POLL_TIMEOUT)
        .delete_webhook()
        .await
        .build();
    let on_error = Arc::new(move |e: RequestError| {
        let log = log.clone();
        async move { log(&format!("getting updates failed: {e}")) }
    });
    dispatcher
        .try_dispatch_with_listener(listener, on_error)
        .await
        .context("Telegram refused the bot (getMe failed); check TELEGRAM_BOT_TOKEN")?;
    app.finish().await;
    Ok(())
}

/// `athena telegram`, wired to the environment: the token, the database,
/// OpenRouter. Every setting is checked before the database is opened.
pub async fn main(model: &str) -> Result<()> {
    let stop = shutdown::listen()?;
    let config = Config::from_env()?;
    let whisper = Whisper::from_env()?.map(Arc::new);
    let health = crate::health::Config::from_env()?;
    let client = agent::client()?;
    let store = Store::open(&store::path())?;
    let health = health.map(|config| Health::production(config, &store));
    let compactor = crate::compaction::from_env(model)?;
    let service =
        Arc::new(Service::new(store.clone(), model, log_warning).with_compactor(compactor));
    let sandboxes = agent::sandboxes_from_env(&store)?;
    let mcp = agent::connect_mcp(&log_warning).await;
    let agent = agent::build_with(
        &client,
        model,
        service.memory(),
        sandboxes.clone(),
        crate::search::WebSearch::from_env(),
        health.clone(),
        &mcp,
    );
    let app = Arc::new(
        Telegram::new(service, store.clone(), agent, Arc::new(log_warning))
            .sandboxes(sandboxes)
            .voice(whisper)
            .health(health),
    );
    let bot = config.bot();
    let mut dispatcher = dispatcher(bot.clone(), app.clone());
    // The scheduler stops claiming jobs on the signal that stops polling
    // (`stop_on`'s task ends), or once polling has ended for any other
    // reason (that task is cancelled below), then finishes its jobs.
    let signalled = stop_on(dispatcher.shutdown_token(), stop);
    let stopper = signalled.abort_handle();
    let scheduler = jobs::scheduler(app.clone(), bot.clone(), store);
    let scheduler = tokio::spawn(scheduler.run(async move {
        let _ = signalled.await;
    }));
    tracing::info!(
        transport = TRANSPORT,
        "polling for messages and running reminders; Ctrl-C or SIGTERM stops"
    );
    let result = serve(&mut dispatcher, bot, &app).await;
    stopper.abort();
    // Job panics are caught and logged inside it; one in the loop itself
    // has already been printed by the runtime.
    let _ = scheduler.await;
    mcp.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::ContextHook;
    use rig_agent::agent::{Agent, AgentBuilder};
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use tokio::sync::{Semaphore, mpsc};

    // ---- pure functions ----

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_binarys_log_is_a_tracing_warning_tagged_with_the_transport() {
        let path = std::env::temp_dir().join(format!("athena-log-{}", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(file)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || log_warning("getting updates failed"));

        let out = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(out.contains("WARN"), "{out}");
        assert!(out.contains("getting updates failed"), "{out}");
        assert!(out.contains("transport=\"telegram\""), "{out}");
    }

    #[test]
    fn only_a_bare_telegram_argument_asks_for_the_bot() {
        assert!(requested(&args(&["telegram"])).unwrap());
        assert!(!requested(&args(&[])).unwrap());
        assert!(!requested(&args(&["sessions"])).unwrap());
        assert!(!requested(&args(&["--user", "telegram:1", "telegram"])).unwrap());
        let err = requested(&args(&["telegram", "hi"])).unwrap_err();
        assert!(err.to_string().contains("takes no arguments"), "{err}");
    }

    #[test]
    fn a_missing_token_is_a_clear_error() {
        for token in [None, Some("".to_string()), Some("  ".to_string())] {
            let err = config(token, None).err().unwrap().to_string();
            assert!(err.contains("TELEGRAM_BOT_TOKEN is not set"), "{err}");
        }
    }

    #[test]
    fn the_api_url_defaults_to_telegram_and_can_point_elsewhere() {
        let default = config(Some("1:abc".into()), None).unwrap().bot();
        assert_eq!(default.api_url().as_str(), "https://api.telegram.org/");
        assert_eq!(default.token(), "1:abc");

        let custom = config(Some("1:abc".into()), Some("http://127.0.0.1:9/tg/".into()))
            .unwrap()
            .bot();
        // The trailing slash is dropped, so teloxide does not build `//bot...`.
        assert_eq!(custom.api_url().as_str(), "http://127.0.0.1:9/tg");
    }

    #[test]
    fn an_api_url_teloxide_would_panic_on_is_refused() {
        for bad in ["localhost:8081", "mailto:x@y", "ftp://host", "not a url"] {
            let err = config(Some("1:abc".into()), Some(bad.into()))
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("TELEGRAM_API_URL"), "{bad}: {err}");
        }
    }

    #[test]
    fn messages_parse_into_commands_or_prompts() {
        for (text, expected) in [
            ("hello", Input::Text("hello")),
            ("/start", Input::Start),
            ("/help", Input::Help),
            ("/new", Input::New(None)),
            ("/new   ", Input::New(None)),
            ("/new my notes ", Input::New(Some("my notes"))),
            ("/new@athena_bot work", Input::New(Some("work"))),
            ("/sessions", Input::Sessions),
            ("/switch", Input::Switch(None)),
            ("/switch\twork", Input::Switch(Some("work"))),
            ("/usage", Input::Usage),
            ("/link", Input::Link(None)),
            ("/link@athena_bot  my-api ", Input::Link(Some("my-api"))),
            ("/frobnicate x", Input::Unknown("frobnicate")),
            ("/", Input::Text("/")),
            ("/usr/bin is a path", Input::Text("/usr/bin is a path")),
            ("/@bot", Input::Text("/@bot")),
            (" /new x", Input::Text(" /new x")),
        ] {
            assert_eq!(parse(text), expected, "{text:?}");
        }
    }

    #[test]
    fn a_picked_name_is_the_first_free_chat_number() {
        assert_eq!(free_name(&[]), "chat-1");
        assert_eq!(free_name(&["default".into()]), "chat-2");
        assert_eq!(
            free_name(&["default".into(), "chat-2".into(), "chat-3".into()]),
            "chat-4"
        );
    }

    #[test]
    fn the_command_menu_is_what_telegram_accepts() {
        for (name, what) in COMMANDS {
            assert!((1..=32).contains(&name.len()), "{name}");
            let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
            assert!(name.chars().all(allowed), "{name}");
            assert!((3..=256).contains(&what.chars().count()), "{what}");
            // Every menu entry is a command the parser knows.
            assert!(!matches!(parse(&format!("/{name}")), Input::Unknown(_)));
        }
        assert_eq!(bot_commands().len(), COMMANDS.len());
        assert_eq!(bot_commands()[0].command, "new");
    }

    fn units(chunks: &[String]) -> Vec<usize> {
        chunks.iter().map(|c| utf16_len(c)).collect()
    }

    #[test]
    fn a_short_reply_is_one_message_unchanged() {
        assert_eq!(split("hello\nworld", 4096), ["hello\nworld"]);
        let exact = "a".repeat(4096);
        assert_eq!(split(&exact, 4096), [exact]);
    }

    #[test]
    fn a_long_reply_is_cut_at_a_line_break_in_the_second_half() {
        let text = "one two three\nfour five six\nseven";
        assert_eq!(split(text, 20), ["one two three", "four five six\nseven"]);
    }

    #[test]
    fn a_blank_line_is_preferred_to_a_later_line_break() {
        let text = "aaaa aaaa aaaa\n\nbbbb\ncccc dddd";
        assert_eq!(split(text, 24), ["aaaa aaaa aaaa", "bbbb\ncccc dddd"]);
        // Only in the first half of the window, it does not count.
        let early = "aa\n\nbbbbbbbbbb cccc dddd";
        assert_eq!(split(early, 16), ["aa\n\nbbbbbbbbbb", "cccc dddd"]);
    }

    #[test]
    fn without_a_late_line_break_it_cuts_at_whitespace() {
        // The only line break is in the first half of the window.
        let text = "ab\ncdefghij klmnop qrs";
        assert_eq!(split(text, 16), ["ab\ncdefghij", "klmnop qrs"]);
    }

    #[test]
    fn without_any_break_it_cuts_at_the_last_character_that_fits() {
        assert_eq!(split("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn a_cut_never_splits_a_character_and_counts_utf16_units() {
        // Each emoji is 4 bytes of UTF-8 and 2 UTF-16 units; each CJK
        // character 3 bytes and 1 unit.
        let emoji = "😀".repeat(5);
        let parts = split(&emoji, 3);
        assert_eq!(parts, ["😀", "😀", "😀", "😀", "😀"]);

        // 4097 units with a surrogate pair straddling the limit.
        let straddle = format!("{}😀", "a".repeat(4095));
        let parts = split(&straddle, 4096);
        assert_eq!(parts, ["a".repeat(4095), "😀".to_string()]);

        let cjk = "漢字".repeat(3000);
        let parts = split(&cjk, 4096);
        assert_eq!(units(&parts), [4096, 1904]);
        assert_eq!(parts.concat(), cjk);
    }

    #[test]
    fn whitespace_only_chunks_are_not_sent() {
        assert!(split("", 10).is_empty());
        assert!(split("\n\n\n\n\n\n\n\n\n\n\n\n", 4).is_empty());
        assert_eq!(split("abcdef\n\n\n\n\n\nghij", 6), ["abcdef", "ghij"]);
    }

    #[test]
    fn every_chunk_fits_and_nothing_but_separators_is_lost() {
        let text = (0..2000)
            .map(|i| format!("line {i}: {}", "word ".repeat(i % 13)))
            .collect::<Vec<_>>()
            .join("\n");
        let parts = split(&text, 500);
        assert!(parts.len() > 10);
        assert!(units(&parts).iter().all(|&n| n <= 500));
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        assert_eq!(squash(&parts.concat()), squash(&text));
    }

    #[test]
    fn a_reply_is_capped_at_max_chunks_with_a_note() {
        assert_eq!(chunks("hi"), [Chunk::plain("hi")]);
        assert_eq!(
            chunks(" \n"),
            [Chunk::plain("(The model sent an empty reply.)")]
        );
        let huge = "x".repeat(MESSAGE_LIMIT * 10);
        let parts = chunks(&huge);
        assert_eq!(parts.len(), MAX_CHUNKS);
        let note = &parts[MAX_CHUNKS - 1];
        let cut_short = note.text.starts_with("(Reply cut short: 3 more");
        assert!(cut_short, "{note:?}");
        assert!(note.entities.is_empty());
        let exactly = "x".repeat(MESSAGE_LIMIT * MAX_CHUNKS);
        assert_eq!(chunks(&exactly).len(), MAX_CHUNKS);
        assert!(!chunks(&exactly).last().unwrap().text.starts_with('('));
    }

    #[test]
    fn every_service_error_has_a_short_reply_without_its_detail() {
        let secret = || anyhow::anyhow!("user_SECRET upstream said no");
        for (error, reply) in [
            (service::Error::NotFound, "That session no longer exists"),
            (
                service::Error::AlreadyExists("x".into()),
                "You already have a session named `x`. /switch x",
            ),
            (service::Error::Invalid("bad name".into()), "bad name"),
            (
                service::Error::Conflict("moved".into()),
                "Send your message again",
            ),
            (service::Error::Model(secret()), "The model failed"),
            (service::Error::Storage(secret()), FAILED),
        ] {
            let text = reply_for(&error);
            assert!(text.starts_with(reply) || text.contains(reply), "{text}");
            assert!(!text.contains("SECRET"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_request_told_to_retry_after_is_retried_a_bounded_number_of_times() {
        let calls = Arc::new(Mutex::new(0));
        let limited = || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err::<(), _>(RequestError::RetryAfter(
                    teloxide::types::Seconds::from_seconds(0),
                ))
            }
        };
        let result = retrying(3, limited).await;
        assert!(matches!(result, Err(RequestError::RetryAfter(_))));
        assert_eq!(*calls.lock().unwrap(), 3);

        let flaky = Arc::new(Mutex::new(vec![
            Ok(7),
            Err(RequestError::RetryAfter(
                teloxide::types::Seconds::from_seconds(0),
            )),
        ]));
        let result = retrying(3, || {
            let next = flaky.lock().unwrap().pop().unwrap();
            async move { next }
        })
        .await;
        assert_eq!(result.unwrap(), 7);

        // Any other error is returned at once.
        let tries = Arc::new(Mutex::new(0));
        let result = retrying(3, || {
            *tries.lock().unwrap() += 1;
            async { Err::<(), _>(RequestError::Api(teloxide::ApiError::BotBlocked)) }
        })
        .await;
        assert!(matches!(result, Err(RequestError::Api(_))));
        assert_eq!(*tries.lock().unwrap(), 1);
    }

    // ---- the logic, against a recording chat and the mock model ----

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Typing,
        Say(String),
        /// A message sent with formatting: its text and entities.
        Formatted(String, Vec<MessageEntity>),
        /// A file sent, or tried: its name, kind and caption.
        Sent(String, Kind, Option<String>),
    }

    impl Event {
        fn said(&self) -> Option<&str> {
            match self {
                Event::Say(text) => Some(text),
                _ => None,
            }
        }
    }

    #[derive(Clone)]
    struct Recorder {
        events: mpsc::UnboundedSender<Event>,
        /// Every call fails.
        fail: bool,
        /// Sending text fails as Telegram does when the user blocked the bot.
        blocked: bool,
        /// What each file id downloads as; any other id fails.
        files: Arc<std::collections::HashMap<String, Vec<u8>>>,
        /// Sending a photo fails, as Telegram refuses some.
        refuse_photos: bool,
        /// Sending formatted text fails with an error of this kind.
        formatting: Formatting,
    }

    /// How a [`Recorder`] answers a message with entities.
    #[derive(Clone, Copy, PartialEq)]
    enum Formatting {
        Accepted,
        /// Telegram says "Bad Request": the entities are refused.
        Refused,
        /// The send fails some other way, as a block or a timeout does.
        Broken,
    }

    impl Chat for Recorder {
        async fn typing(&self) -> Result<()> {
            self.events.send(Event::Typing).unwrap();
            if self.fail {
                bail!("typing refused");
            }
            Ok(())
        }

        async fn say(&self, text: &str) -> Result<()> {
            self.events.send(Event::Say(text.to_string())).unwrap();
            if self.blocked {
                return Err(RequestError::Api(ApiError::BotBlocked).into());
            }
            if self.fail {
                bail!("blocked by the user");
            }
            Ok(())
        }

        async fn say_formatted(&self, text: &str, entities: &[MessageEntity]) -> Result<()> {
            let event = Event::Formatted(text.to_string(), entities.to_vec());
            self.events.send(event).unwrap();
            match self.formatting {
                Formatting::Accepted => Ok(()),
                Formatting::Refused => {
                    Err(Refused("Bad Request: can't parse entities".into()).into())
                }
                Formatting::Broken => bail!("connection reset"),
            }
        }

        async fn download(&self, id: &str, _limit: usize) -> Result<Vec<u8>> {
            self.files.get(id).cloned().context("file not found")
        }

        async fn send_file(&self, attachment: &Attachment) -> Result<()> {
            let (name, caption) = (attachment.name.clone(), attachment.caption.clone());
            self.events
                .send(Event::Sent(name, attachment.kind, caption))
                .unwrap();
            if self.refuse_photos && attachment.kind == Kind::Photo {
                return Err(Refused("Bad Request: PHOTO_INVALID_DIMENSIONS".into()).into());
            }
            if self.fail {
                bail!("connection reset");
            }
            Ok(())
        }
    }

    fn recorder() -> (Recorder, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Recorder {
                events,
                fail: false,
                blocked: false,
                files: Arc::default(),
                refuse_photos: false,
                formatting: Formatting::Accepted,
            },
            rx,
        )
    }

    type Logged = Arc<Mutex<Vec<String>>>;

    struct Harness<R> {
        app: Arc<Telegram<R>>,
        store: Store,
        logged: Logged,
    }

    fn harness_with<R: Run + 'static>(make: impl FnOnce(&Service) -> R) -> Harness<R> {
        harness_over(Store::open_in_memory().unwrap(), make, None)
    }

    /// [`harness_with`] on `store`, with Google Health `health` (built on
    /// the same store).
    fn harness_over<R: Run + 'static>(
        store: Store,
        make: impl FnOnce(&Service) -> R,
        health: Option<Arc<Health>>,
    ) -> Harness<R> {
        let service = Service::new(store.clone(), "test/model", |_| {});
        let agent = make(&service);
        let logged = Logged::default();
        let sink = logged.clone();
        let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
        // Long enough that no renewal lands mid-test; one test shortens it.
        // Albums close quickly, so their tests do not wait two seconds.
        let app = Telegram::new(Arc::new(service), store.clone(), agent, log)
            .typing_every(Duration::from_secs(3600))
            .album_wait(Duration::from_millis(200))
            .health(health);
        Harness {
            app: Arc::new(app),
            store,
            logged,
        }
    }

    fn mock(service: &Service, turns: Vec<MockTurn>) -> Agent {
        let model = MockCompletionModel::new(turns);
        agent::configure(AgentBuilder::new(model).memory(service.memory()))
    }

    fn harness(turns: Vec<MockTurn>) -> Harness<Agent> {
        harness_with(|s| mock(s, turns))
    }

    fn from(user: u64, text: &str) -> Incoming {
        Incoming {
            chat_id: user as i64,
            private: true,
            user_id: Some(user),
            text: Some(text.into()),
            files: vec![],
            album: None,
            voice: None,
        }
    }

    impl<R: Run + 'static> Harness<R> {
        /// Send `text` as `user`, wait for every turn, return what was sent back.
        async fn send(&self, user: u64, text: &str) -> Vec<Event> {
            let (chat, mut rx) = recorder();
            self.app.handle(chat, from(user, text)).await;
            self.app.finish().await;
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            events
        }

        /// Send `text` as `user` and return what came back before any turn
        /// it started has finished.
        async fn now(&self, user: u64, text: &str) -> Vec<Event> {
            let (chat, mut rx) = recorder();
            self.app.handle(chat, from(user, text)).await;
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            events
        }

        /// The one message sent back.
        async fn reply(&self, user: u64, text: &str) -> String {
            let events = self.send(user, text).await;
            assert_eq!(events.len(), 1, "expected one reply, got {events:?}");
            events[0].said().unwrap().to_string()
        }

        fn user(&self, id: u64) -> User {
            self.store.user(TRANSPORT, &id.to_string()).unwrap()
        }

        /// Session name -> message count, for one Telegram user.
        fn sessions(&self, id: u64) -> Vec<(String, i64)> {
            self.store
                .sessions(&self.user(id))
                .unwrap()
                .into_iter()
                .map(|s| (s.session.name, s.messages))
                .collect()
        }

        fn selected(&self, id: u64) -> Option<String> {
            self.store
                .selected_session(&self.user(id))
                .unwrap()
                .map(|s| s.name)
        }

        fn logged(&self) -> Vec<String> {
            self.logged.lock().unwrap().clone()
        }
    }

    fn said(text: &str) -> Event {
        Event::Say(text.into())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_message_creates_the_user_and_their_default_session() {
        let h = harness(vec![MockTurn::text("hello back")]);

        let events = h.send(42, "hello").await;

        assert_eq!(events, [Event::Typing, said("hello back")]);
        assert_eq!(events[0].said(), None);
        assert_eq!(h.sessions(42), [("default".to_string(), 2)]);
        assert_eq!(h.selected(42), None);
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test]
    async fn link_is_a_direct_command_and_never_reassigns_another_user() {
        let h = harness(vec![]);
        assert_eq!(h.reply(42, "/link").await, LINK_USAGE);
        let linked = h.reply(42, "/link my-api").await;
        assert!(linked.contains("X-Athena-User: my-api"), "{linked}");
        assert_eq!(h.reply(42, "/link my-api").await, linked);
        assert_eq!(
            h.store.user("http", "my-api").unwrap().id(),
            h.user(42).id()
        );
        let refused = h.reply(43, "/link my-api").await;
        let conflict = "already belongs to another user";
        assert!(refused.contains(conflict), "{refused}");
        let invalid = h.reply(42, "/link café").await;
        assert!(invalid.contains("printable ASCII"), "{invalid}");
        assert!(h.sessions(42).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_creates_and_selects_a_session_and_messages_go_there() {
        let h = harness(vec![MockTurn::text("in work")]);

        let reply = h.reply(1, "/new work").await;
        h.send(1, "hi").await;

        assert!(reply.contains("Started session `work`"), "{reply}");
        assert_eq!(h.selected(1).as_deref(), Some("work"));
        assert_eq!(h.sessions(1), [("work".to_string(), 2)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_without_a_name_picks_one_and_a_taken_name_is_refused() {
        let h = harness(vec![]);

        h.reply(1, "/sessions").await; // creates `default`
        let picked = h.reply(1, "/new").await;
        let again = h.reply(1, "/new chat-2").await;
        let two_lines = h.reply(1, "/new a\nb").await;

        assert!(picked.contains("`chat-2`"), "{picked}");
        assert_eq!(
            again,
            "You already have a session named `chat-2`. /switch chat-2 to use it."
        );
        assert_eq!(two_lines, "A session name must fit on one line.");
        assert_eq!(h.selected(1).as_deref(), Some("chat-2"));
        assert_eq!(h.sessions(1).len(), 2);
        // A refused request is logged with the user id, not the text.
        assert!(
            h.logged()
                .iter()
                .all(|l| l.starts_with("telegram user 1: "))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switch_moves_between_existing_sessions_and_never_creates_one() {
        let h = harness(vec![MockTurn::text("a1"), MockTurn::text("d1")]);
        h.reply(1, "/new a").await;
        h.send(1, "to a").await;

        let missing = h.reply(1, "/switch nope").await;
        let usage = h.reply(1, "/switch").await;
        let back = h.reply(1, "/switch default").await;
        h.send(1, "to default").await;
        let listed = h.reply(1, "/sessions").await;

        let m = &missing;
        assert!(m.starts_with("You have no session named"), "{m}");
        assert_eq!(usage, SWITCH_USAGE);
        assert_eq!(back, "Switched to `default` (0 messages).");
        assert_eq!(
            listed,
            "Your sessions:\n  a (2 messages)\n* default (2 messages)"
        );
        assert_eq!(
            h.sessions(1),
            [("a".to_string(), 2), ("default".to_string(), 2)]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn help_start_usage_and_unknown_commands_answer_without_the_model() {
        let h = harness(vec![MockTurn::text("ok")]);

        let start = h.reply(1, "/start").await;
        let help = h.reply(1, "/help").await;
        let none = h.reply(1, "/usage").await;
        h.send(1, "hi").await;
        let usage = h.reply(1, "/usage").await;
        let unknown = h.reply(1, "/frobnicate").await;

        assert!(start.contains("You are in session `default`"), "{start}");
        assert!(start.contains("/switch - "), "{start}");
        assert_eq!(start, help);
        assert_eq!(none, "No turns yet.");
        assert_eq!(
            usage,
            "default: 1 turns, 1 model calls, 0 tokens in (0 cached), 0 out"
        );
        assert!(unknown.starts_with("Unknown command /frob"), "{unknown}");
        assert!(unknown.contains("/new - "), "{unknown}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_users_never_see_each_others_sessions() {
        let h = harness(vec![MockTurn::text("for one"), MockTurn::text("for two")]);

        h.reply(1, "/new secret").await;
        h.send(1, "mine").await;
        let theirs = h.reply(2, "/switch secret").await;
        h.send(2, "hello").await;

        assert!(theirs.starts_with("You have no session named"), "{theirs}");
        assert_eq!(h.sessions(1), [("secret".to_string(), 2)]);
        assert_eq!(h.sessions(2), [("default".to_string(), 2)]);
        assert_eq!(h.selected(2), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_reply_arrives_in_order_in_chunks_that_fit() {
        let long = format!("{}\n{}", "a".repeat(4000), "b".repeat(4000));
        let h = harness(vec![MockTurn::text(long)]);

        let events = h.send(1, "write a lot").await;

        assert_eq!(
            events,
            [
                Event::Typing,
                said(&"a".repeat(4000)),
                said(&"b".repeat(4000))
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_failure_is_logged_and_answered_briefly() {
        let h = harness(vec![MockTurn::error("upstream user_SECRET unavailable")]);

        let events = h.send(7, "hi").await;

        assert_eq!(
            events,
            [
                Event::Typing,
                said("The model failed to answer. Try again in a moment.")
            ]
        );
        let logged = h.logged();
        assert_eq!(logged.len(), 1);
        let first = &logged[0];
        assert!(first.starts_with("telegram user 7: turn failed"), "{first}");
        assert!(logged[0].contains("user_SECRET"), "{logged:?}");
        // The user can talk again: the failed turn released them.
        assert!(!lock(&h.app.busy).contains(&7));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_storage_failure_before_a_turn_is_answered_and_releases_the_user() {
        let h = harness(vec![]);
        h.store
            .db_for_tests()
            .execute_batch("DROP TABLE selected_sessions")
            .unwrap();

        let reply = h.reply(3, "hello").await;

        assert_eq!(reply, FAILED);
        assert!(h.logged()[0].starts_with("telegram user 3: storage: "));
        assert!(!lock(&h.app.busy).contains(&3));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn groups_senderless_posts_and_non_text_messages_are_handled_safely() {
        let h = harness(vec![]);
        let group = Incoming {
            chat_id: -100,
            private: false,
            user_id: Some(1),
            text: Some("hello everyone".into()),
            files: vec![],
            album: None,
            voice: None,
        };
        let channel = Incoming {
            chat_id: 5,
            private: true,
            user_id: None,
            text: Some("post".into()),
            files: vec![],
            album: None,
            voice: None,
        };
        let sticker = Incoming {
            text: None,
            ..from(9, "")
        };

        for (incoming, expected) in [
            (group, vec![]),
            (channel, vec![]),
            (sticker, vec![said(NOT_TEXT)]),
        ] {
            let (chat, mut rx) = recorder();
            h.app.handle(chat, incoming).await;
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            assert_eq!(events, expected);
        }
        // Nobody was stored for the ignored ones, and no text was logged.
        let users: i64 = h
            .store
            .db_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM users WHERE transport = 'telegram'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(users, 0);
        let logged = h.logged();
        assert_eq!(
            logged,
            [
                "ignoring a message in chat -100: only private chats are served",
                "ignoring a message with no sender in chat 5"
            ]
        );
    }

    /// What `chat` was asked to do while the bot handled `hi` as user 1.
    async fn events_for(h: &Harness<Agent>, chat: Recorder, mut rx: EventRx) -> Vec<Event> {
        h.app.handle(chat, from(1, "hi")).await;
        h.app.finish().await;
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        seen
    }

    type EventRx = mpsc::UnboundedReceiver<Event>;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_markdown_reply_is_sent_as_text_with_entities() {
        let h = harness(vec![MockTurn::text("**Hi** 😀 `there`\n\nplain")]);
        let (chat, rx) = recorder();

        let seen = events_for(&h, chat, rx).await;

        let entities = vec![MessageEntity::bold(0, 2), MessageEntity::code(6, 5)];
        assert_eq!(
            seen,
            [
                Event::Typing,
                Event::Formatted("Hi 😀 there\n\nplain".into(), entities)
            ]
        );
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn formatting_telegram_refuses_is_sent_again_as_plain_text() {
        let h = harness(vec![MockTurn::text("**Hi** there")]);
        let (mut chat, rx) = recorder();
        chat.formatting = Formatting::Refused;

        let seen = events_for(&h, chat, rx).await;

        // The text, with the markup already gone, arrives without entities.
        let entities = vec![MessageEntity::bold(0, 2)];
        assert_eq!(
            seen,
            [
                Event::Typing,
                Event::Formatted("Hi there".into(), entities),
                said("Hi there")
            ]
        );
        let logged = h.logged();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(
            logged[0].starts_with("sending formatted text failed"),
            "{logged:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_failure_to_send_formatted_text_is_not_sent_again() {
        let long = format!("**{}**\n\n{}", "a".repeat(4000), "b".repeat(4000));
        let h = harness(vec![MockTurn::text(long)]);
        let (mut chat, rx) = recorder();
        chat.formatting = Formatting::Broken;

        let seen = events_for(&h, chat, rx).await;

        // One attempt at the first chunk, no plain copy, none at the second.
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(matches!(seen[1], Event::Formatted(..)));
        assert_eq!(h.logged(), ["sending a message failed: connection reset"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_sends_are_logged_and_stop_the_remaining_chunks() {
        let long = format!("{}\n{}", "a".repeat(4000), "b".repeat(4000));
        let h = harness(vec![MockTurn::text(long)]);
        let (mut chat, mut rx) = recorder();
        chat.fail = true;

        h.app.handle(chat, from(1, "hi")).await;
        h.app.finish().await;

        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        // One attempt at the first chunk, none at the second.
        assert_eq!(seen, [Event::Typing, said(&"a".repeat(4000))]);
        assert_eq!(
            h.logged(),
            [
                "sending the typing indicator failed: typing refused",
                "sending a message failed: blocked by the user"
            ]
        );
        // The turn itself was saved.
        assert_eq!(h.sessions(1), [("default".to_string(), 2)]);
    }

    /// A model that waits for a permit before answering, and says when it
    /// has started.
    struct Parked {
        inner: Agent,
        started: mpsc::UnboundedSender<()>,
        gate: Arc<Semaphore>,
    }

    impl Run for Parked {
        async fn run(
            &self,
            prompt: &Request,
            conversation: &str,
            context: Option<ContextHook>,
        ) -> Result<rig_agent::agent::PromptResponse, rig_agent::completion::PromptError> {
            self.started.send(()).unwrap();
            self.gate.acquire().await.unwrap().forget();
            self.inner.run(prompt, conversation, context).await
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_message_during_a_turn_is_refused_and_commands_still_answer() {
        let gate = Arc::new(Semaphore::new(0));
        let (started, mut starts) = mpsc::unbounded_channel();
        let h = harness_with(|s| Parked {
            inner: mock(s, vec![MockTurn::text("first reply")]),
            started,
            gate: gate.clone(),
        });
        let (chat, mut rx) = recorder();

        h.app.handle(chat.clone(), from(1, "first")).await;
        starts.recv().await.unwrap(); // the model is working on it
        h.app.handle(chat.clone(), from(1, "second")).await;
        h.app.handle(chat.clone(), from(1, "/sessions")).await;
        // Another user is not held up either.
        let other = h.now(2, "/usage").await;
        gate.add_permits(1);
        // The timeout only turns a second parked turn (no busy guard) into
        // a failure instead of a hang.
        let finished = tokio::time::timeout(Duration::from_secs(30), h.app.finish()).await;
        assert!(finished.is_ok(), "a refused message started a turn");

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert_eq!(
            events,
            [
                Event::Typing,
                said(BUSY),
                said("Your sessions:\n* default (0 messages)"),
                said("first reply"),
            ]
        );
        assert_eq!(other, [said("No turns yet.")]);
        // Only the first message reached the model and the transcript.
        assert_eq!(h.sessions(1), [("default".to_string(), 2)]);
        assert!(starts.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_typing_indicator_is_renewed_until_the_turn_ends() {
        let gate = Arc::new(Semaphore::new(0));
        let (started, _starts) = mpsc::unbounded_channel();
        let h = harness_with(|s| Parked {
            inner: mock(s, vec![MockTurn::text("done")]),
            started,
            gate: gate.clone(),
        });
        let app = Arc::new(
            Arc::into_inner(h.app)
                .unwrap()
                .typing_every(Duration::from_millis(1)),
        );
        let (chat, mut rx) = recorder();

        app.handle(chat, from(1, "slow")).await;
        // The first indicator, then at least two renewals while parked.
        for _ in 0..3 {
            assert_eq!(rx.recv().await, Some(Event::Typing));
        }
        gate.add_permits(1);
        app.finish().await;

        let mut rest = Vec::new();
        while let Ok(e) = rx.try_recv() {
            rest.push(e);
        }
        // The reply comes last, and no indicator follows it.
        assert_eq!(rest.last(), Some(&said("done")));
        assert!(rest[..rest.len() - 1].iter().all(|e| *e == Event::Typing));
    }

    struct Panics;

    impl Run for Panics {
        async fn run(
            &self,
            _: &Request,
            _: &str,
            _: Option<ContextHook>,
        ) -> Result<rig_agent::agent::PromptResponse, rig_agent::completion::PromptError> {
            panic!("the agent blew up")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_turn_is_logged_answered_and_releases_the_user() {
        let h = harness_with(|_| Panics);

        let events = h.send(4, "boom").await;

        assert_eq!(events, [Event::Typing, said(FAILED)]);
        assert!(h.logged()[0].starts_with("telegram user 4: turn panicked"));
        assert!(!lock(&h.app.busy).contains(&4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_while_handling_a_message_is_contained() {
        let log_lines = Logged::default();
        let sink = log_lines.clone();
        let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
        let (chat, mut rx) = recorder();

        isolated(&chat, &log, async { panic!("formatting bug") }).await;
        isolated(&chat, &log, async {}).await;

        assert_eq!(rx.try_recv().unwrap(), said(FAILED));
        assert!(rx.try_recv().is_err());
        let lines = log_lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("formatting bug"), "{lines:?}");
    }

    // ---- files ----

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nphoto";

    fn photo(id: &str, size: u64) -> IncomingFile {
        IncomingFile {
            id: id.into(),
            name: "photo.jpg".into(),
            mime: Some("image/jpeg".into()),
            size,
        }
    }

    /// A message from `user` carrying `file`, captioned `caption`.
    fn with_file(user: u64, caption: Option<&str>, file: IncomingFile) -> Incoming {
        Incoming {
            text: caption.map(str::to_string),
            files: vec![file],
            ..from(user, "")
        }
    }

    /// Handle `incoming` with `chat`, wait for the turn, return the events.
    async fn exchange<R: Run + 'static>(
        h: &Harness<R>,
        chat: Recorder,
        mut rx: mpsc::UnboundedReceiver<Event>,
        incoming: Incoming,
    ) -> Vec<Event> {
        h.app.handle(chat, incoming).await;
        h.app.finish().await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    /// A harness whose model is `model`, so a test can see its requests.
    fn watched(turns: Vec<MockTurn>) -> (Harness<Agent>, MockCompletionModel) {
        let model = MockCompletionModel::new(turns);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        (h, model)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_photo_is_shown_to_the_model_with_its_caption() {
        let (h, model) = watched(vec![MockTurn::text("a cat")]);
        let (mut chat, rx) = recorder();
        chat.files = Arc::new([("f1".to_string(), PNG.to_vec())].into());

        let events = exchange(
            &h,
            chat,
            rx,
            with_file(5, Some("what is it?"), photo("f1", 13)),
        )
        .await;

        assert_eq!(events, [Event::Typing, said("a cat")]);
        let prompt = model.requests()[0].chat_history.last().unwrap().clone();
        let expected = Request {
            text: "what is it?".into(),
            files: vec![File {
                name: "photo.jpg".into(),
                mime: Some("image/jpeg".into()),
                size: PNG.len() as u64,
                image: Some(media::image(PNG)),
                saved: Err("this assistant has no sandbox".into()),
            }],
            outbox: None,
            scheduled: false,
        };
        assert_eq!(prompt, expected.message());
        // The transcript has the note, not the photo.
        assert_eq!(h.sessions(5), [("default".to_string(), 2)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_is_a_prompt_even_without_a_caption_and_its_caption_is_never_a_command() {
        let (h, model) = watched(vec![MockTurn::text("got it"), MockTurn::text("again")]);
        // Nothing to download: with no sandbox, a PDF is not fetched at all.
        let (chat, rx) = recorder();
        let doc = IncomingFile {
            id: "f1".into(),
            name: "../r\u{0}eport.pdf".into(),
            mime: Some("application/pdf".into()),
            size: 4,
        };

        let events = exchange(&h, chat.clone(), rx, with_file(6, None, doc.clone())).await;
        assert_eq!(events, [Event::Typing, said("got it")]);
        let text = model.requests()[0]
            .chat_history
            .last()
            .unwrap()
            .rag_text()
            .unwrap();
        assert_eq!(
            text,
            "[The user attached `r_eport.pdf` (application/pdf, 4 bytes). It is not in your \
             sandbox: this assistant has no sandbox.]"
        );

        let (chat, rx) = recorder();
        let events = exchange(&h, chat, rx, with_file(6, Some("/new x"), doc)).await;
        assert_eq!(events, [Event::Typing, said("again")]);
        assert_eq!(h.selected(6), None);
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_too_large_to_download_is_refused_before_any_turn() {
        let h = harness(vec![]);
        let (chat, rx) = recorder();
        let big = photo("f1", DOWNLOAD_LIMIT as u64 + 1);
        let events = exchange(&h, chat, rx, with_file(7, None, big.clone())).await;
        assert_eq!(events, [said(&too_big(&big))]);
        assert_eq!(
            too_big(&big),
            "`photo.jpg` is 21 MB; I can only receive files up to 20 MB."
        );
        assert!(h.sessions(7).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_cannot_be_downloaded_is_described_to_the_model_and_logged() {
        let (h, model) = watched(vec![MockTurn::text("I could not open it")]);
        let (chat, rx) = recorder();
        let events = exchange(&h, chat, rx, with_file(8, Some("see"), photo("gone", 10))).await;
        assert_eq!(events, [Event::Typing, said("I could not open it")]);
        let text = model.requests()[0]
            .chat_history
            .last()
            .unwrap()
            .rag_text()
            .unwrap();
        let note = "(image/jpeg, 10 bytes). It is not in your sandbox: it could not be \
                    downloaded from Telegram.]";
        assert!(text.ends_with(note), "{text}");
        assert_eq!(
            h.logged(),
            ["downloading a file from Telegram failed: file not found"]
        );
    }

    // ---- voice notes ----

    const OGG: &[u8] = b"OggS\0\x02 a short voice note";
    const CF_TOKEN: &str = "cf-test-token-not-real";

    fn voice_note(id: &str, size: u64, seconds: u32) -> IncomingVoice {
        IncomingVoice {
            id: id.into(),
            size,
            seconds,
        }
    }

    /// A voice note from `user`, captioned `caption`.
    fn spoke(user: u64, caption: Option<&str>, voice: IncomingVoice) -> Incoming {
        Incoming {
            text: caption.map(str::to_string),
            voice: Some(voice),
            ..from(user, "")
        }
    }

    /// A harness that transcribes with a fake Cloudflare, and that fake.
    async fn listening(
        turns: Vec<MockTurn>,
    ) -> (
        Harness<Agent>,
        MockCompletionModel,
        voice::fake::FakeCloudflare,
    ) {
        let cf = voice::fake::FakeCloudflare::start().await;
        let (mut h, model) = watched(turns);
        let whisper = Whisper::new("acct1", CF_TOKEN, &cf.url).unwrap();
        Arc::get_mut(&mut h.app).unwrap().whisper = Some(Arc::new(whisper));
        (h, model, cf)
    }

    /// A recorder that downloads `id` as [`OGG`].
    fn with_audio(id: &str) -> (Recorder, mpsc::UnboundedReceiver<Event>) {
        let (mut chat, rx) = recorder();
        chat.files = Arc::new([(id.to_string(), OGG.to_vec())].into());
        (chat, rx)
    }

    #[test]
    fn a_voice_notes_prompt_is_its_caption_then_its_transcript() {
        assert_eq!(spoken("", "log squats"), "log squats");
        assert_eq!(spoken(" \n", "log squats"), "log squats");
        assert_eq!(spoken(" leg day: ", "log squats"), "leg day:\n\nlog squats");
    }

    #[test]
    fn a_voice_note_over_the_size_or_length_limit_is_refused_by_name() {
        assert_eq!(
            unheard(&voice_note("v", VOICE_LIMIT as u64, VOICE_SECONDS)),
            None
        );
        // Size unknown: only the length is checked before the download.
        assert_eq!(unheard(&voice_note("v", 0, 1)), None);
        assert_eq!(
            unheard(&voice_note("v", VOICE_LIMIT as u64 + 1, 1)).unwrap(),
            "That voice note is 3 MB; I can transcribe up to 2 MB."
        );
        assert_eq!(
            unheard(&voice_note("v", 10, VOICE_SECONDS + 1)).unwrap(),
            "That voice note is 6 minutes long; I can transcribe up to 5 minutes."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_voice_note_is_transcribed_and_answered_as_a_text_turn() {
        let (h, model, cf) =
            listening(vec![MockTurn::text("logged"), MockTurn::text("again")]).await;
        cf.answer(200, &voice::fake::heard(" log five sets of squats "));
        let (chat, rx) = with_audio("v1");

        let note = spoke(20, Some(" leg day: "), voice_note("v1", 9, 4));
        let events = exchange(&h, chat, rx, note).await;

        assert_eq!(events, [Event::Typing, said("logged")]);
        let prompt = model.requests()[0].chat_history.last().unwrap().clone();
        assert_eq!(
            prompt.rag_text().unwrap(),
            "leg day:\n\nlog five sets of squats"
        );
        let seen = cf.seen();
        assert_eq!(seen[0].body["audio"], media::base64(OGG));
        assert_eq!(
            seen[0].authorization.as_deref(),
            Some(&*format!("Bearer {CF_TOKEN}"))
        );
        // The transcript is stored as the user's message; the audio is not.
        assert_eq!(h.sessions(20), [("default".to_string(), 2)]);
        let stored: Vec<String> = h
            .store
            .db_for_tests()
            .prepare("SELECT json FROM messages")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(stored[0].contains("log five sets of squats"), "{stored:?}");
        assert!(stored.iter().all(|m| !m.contains(&media::base64(OGG))));

        // A caption that looks like a command is part of the prompt.
        let (chat, rx) = with_audio("v1");
        let events = exchange(
            &h,
            chat,
            rx,
            spoke(20, Some("/new x"), voice_note("v1", 9, 4)),
        )
        .await;
        assert_eq!(events, [Event::Typing, said("again")]);
        let prompt = model.requests()[1].chat_history.last().unwrap().clone();
        assert_eq!(prompt.rag_text().unwrap(), "/new x\n\nhello there");
        assert_eq!(h.selected(20), None);
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_cloudflare_a_voice_note_is_not_a_prompt() {
        let h = harness(vec![]);
        let (chat, rx) = with_audio("v1");
        let events = exchange(&h, chat, rx, spoke(21, Some("hi"), voice_note("v1", 9, 4))).await;
        assert_eq!(events, [said(NOT_TEXT)]);
        assert!(h.sessions(21).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_cloudflare_other_messages_name_voice_notes_and_big_notes_are_refused() {
        let (h, _model, cf) = listening(vec![]).await;
        let sticker = Incoming {
            text: None,
            ..from(22, "")
        };
        let (chat, rx) = recorder();
        assert_eq!(
            exchange(&h, chat, rx, sticker).await,
            [said(NOT_TEXT_OR_VOICE)]
        );

        for note in [
            voice_note("v1", VOICE_LIMIT as u64 + 1, 4),
            voice_note("v1", 9, VOICE_SECONDS + 1),
        ] {
            let (chat, rx) = with_audio("v1");
            let events = exchange(&h, chat, rx, spoke(22, None, note.clone())).await;
            assert_eq!(events, [said(&unheard(&note).unwrap())]);
        }
        assert!(cf.seen().is_empty());
        assert!(h.sessions(22).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_note_that_cannot_be_fetched_or_transcribed_is_answered_and_logged() {
        let (h, model, cf) = listening(vec![]).await;
        cf.answer(
            401,
            r#"{"success": false, "errors": [{"message": "Authentication error"}]}"#,
        );
        let (chat, rx) = with_audio("v1");
        let events = exchange(&h, chat, rx, spoke(23, None, voice_note("v1", 9, 4))).await;
        assert_eq!(events, [Event::Typing, said(NOT_HEARD)]);

        let (chat, rx) = with_audio("v1");
        let events = exchange(&h, chat, rx, spoke(23, None, voice_note("gone", 9, 4))).await;
        assert_eq!(events, [Event::Typing, said(NOT_HEARD)]);

        assert_eq!(cf.seen().len(), 1);
        assert!(model.requests().is_empty());
        assert_eq!(h.sessions(23), [("default".to_string(), 0)]);
        assert!(!lock(&h.app.busy).contains(&23));
        assert_eq!(
            h.logged(),
            [
                "transcribing a voice note failed: Cloudflare answered HTTP 401 without success: \
                 Authentication error",
                "transcribing a voice note failed: downloading it from Telegram failed: \
                 file not found"
            ]
        );
        assert!(h.logged().iter().all(|l| !l.contains(CF_TOKEN)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_note_is_transcribed_at_a_time_and_its_sender_is_busy_meanwhile() {
        let (h, _model, cf) = listening(vec![MockTurn::text("one"), MockTurn::text("two")]).await;
        cf.park();
        let (first, mut first_rx) = with_audio("v1");
        let (second, mut second_rx) = with_audio("v1");

        h.app
            .handle(first, spoke(30, None, voice_note("v1", 9, 4)))
            .await;
        h.app
            .handle(second, spoke(31, None, voice_note("v1", 9, 4)))
            .await;
        cf.arrived(1).await;

        // The first note holds the one permit, so the second waits for it.
        assert_eq!(h.app.voices.available_permits(), 0);
        assert_eq!(cf.seen().len(), 1);
        assert_eq!(h.now(30, "hi").await, [said(BUSY)]);

        cf.release(2);
        h.app.finish().await;
        assert_eq!(cf.seen().len(), 2);
        let mut replies = Vec::new();
        for rx in [&mut first_rx, &mut second_rx] {
            while let Ok(e) = rx.try_recv() {
                replies.extend(e.said().map(str::to_string));
            }
        }
        replies.sort();
        assert_eq!(replies, ["one", "two"]);
        assert_eq!(h.app.voices.available_permits(), VOICES);
    }

    /// An agent whose tools queued `attachments` during the turn.
    struct Sends {
        inner: Agent,
        attachments: Vec<Attachment>,
    }

    impl Run for Sends {
        async fn run(
            &self,
            request: &Request,
            conversation: &str,
            context: Option<ContextHook>,
        ) -> Result<rig_agent::agent::PromptResponse, rig_agent::completion::PromptError> {
            let outbox = request.outbox.as_ref().unwrap();
            for attachment in &self.attachments {
                outbox.push(attachment.clone()).unwrap();
            }
            self.inner.run(request, conversation, context).await
        }
    }

    fn attachment(name: &str, kind: Kind, caption: Option<&str>) -> Attachment {
        Attachment {
            name: name.into(),
            bytes: PNG.to_vec(),
            kind,
            caption: caption.map(str::to_string),
        }
    }

    fn sends(turns: Vec<MockTurn>) -> Harness<Sends> {
        harness_with(|s| Sends {
            inner: mock(s, turns),
            attachments: vec![
                attachment("shot.png", Kind::Photo, Some("the page")),
                attachment("report.csv", Kind::Document, None),
            ],
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_the_tools_queued_follow_the_reply() {
        let h = sends(vec![MockTurn::text("here you go")]);
        let (chat, rx) = recorder();
        let events = exchange(&h, chat, rx, from(9, "screenshot please")).await;
        assert_eq!(
            events,
            [
                Event::Typing,
                said("here you go"),
                Event::Sent("shot.png".into(), Kind::Photo, Some("the page".into())),
                Event::Sent("report.csv".into(), Kind::Document, None),
            ]
        );
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_photo_goes_again_as_a_file_and_an_unsendable_file_is_mentioned() {
        let h = sends(vec![MockTurn::text("one"), MockTurn::text("two")]);
        let (mut chat, rx) = recorder();
        chat.refuse_photos = true;
        let events = exchange(&h, chat, rx, from(10, "go")).await;
        assert_eq!(
            events[2..],
            [
                Event::Sent("shot.png".into(), Kind::Photo, Some("the page".into())),
                Event::Sent("shot.png".into(), Kind::Document, Some("the page".into())),
                Event::Sent("report.csv".into(), Kind::Document, None),
            ]
        );
        assert_eq!(
            h.logged(),
            [
                "sending a photo failed, sending it as a file: Telegram refused it: Bad Request: \
              PHOTO_INVALID_DIMENSIONS"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_lost_on_the_way_is_not_sent_again_but_mentioned() {
        let h = sends(vec![MockTurn::text("one")]);
        let (mut chat, rx) = recorder();
        chat.fail = true;
        let events = exchange(&h, chat, rx, from(10, "go")).await;
        // The photo is not retried as a file: it may have arrived.
        assert_eq!(
            events[2..],
            [
                Event::Sent("shot.png".into(), Kind::Photo, Some("the page".into())),
                said("(I could not send you `shot.png`.)"),
                Event::Sent("report.csv".into(), Kind::Document, None),
                said("(I could not send you `report.csv`.)"),
            ]
        );
        let lost = "sending a file failed: connection reset".to_string();
        assert_eq!(h.logged().iter().filter(|l| **l == lost).count(), 2);
    }

    // ---- albums ----

    /// Item `n` of album `album` from `user`: photo `p<n>`, captioned.
    fn album_item(user: u64, album: &str, n: usize, caption: Option<&str>) -> Incoming {
        Incoming {
            album: Some(album.into()),
            ..with_file(user, caption, photo(&format!("p{n}"), 13))
        }
    }

    /// A recorder that can download photos `p0` to `p<n - 1>`.
    fn album_chat(n: usize) -> (Recorder, mpsc::UnboundedReceiver<Event>) {
        let (mut chat, rx) = recorder();
        chat.files = Arc::new((0..n).map(|i| (format!("p{i}"), PNG.to_vec())).collect());
        (chat, rx)
    }

    /// Hand every item to the bot, then wait for the album's turn.
    async fn album<R: Run + 'static>(
        h: &Harness<R>,
        chat: Recorder,
        mut rx: mpsc::UnboundedReceiver<Event>,
        items: Vec<Incoming>,
    ) -> Vec<Event> {
        for item in items {
            h.app.handle(chat.clone(), item).await;
        }
        h.app.finish().await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_album_is_one_turn_with_every_photo_and_its_caption() {
        let (h, model) = watched(vec![MockTurn::text("three cats")]);
        let (chat, rx) = album_chat(3);
        let items = vec![
            album_item(20, "a1", 0, None),
            // Telegram puts the caption on whichever item it was typed on.
            album_item(20, "a1", 1, Some("compare these")),
            album_item(20, "a1", 2, None),
        ];

        let events = album(&h, chat, rx, items).await;

        assert_eq!(events, [Event::Typing, said("three cats")]);
        assert_eq!(model.requests().len(), 1);
        let prompt = model.requests()[0].chat_history.last().unwrap().clone();
        let text = prompt.rag_text().unwrap();
        let start = "compare these\n\n[The user attached";
        assert!(text.starts_with(start), "{text}");
        assert_eq!(text.matches("It is shown to you below.").count(), 3);
        let json = serde_json::to_value(&prompt).unwrap();
        let images = json["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["type"] == "image");
        assert_eq!(images.count(), 3);
        assert!(lock(&h.app.albums).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn albums_are_per_user_and_a_busy_user_is_told() {
        let (h, model) = watched(vec![MockTurn::text("one"), MockTurn::text("one")]);
        // The same album id from two users: two albums, two turns.
        let (chat, rx) = album_chat(2);
        let items = vec![album_item(21, "a1", 0, None), album_item(25, "a1", 1, None)];
        let events = album(&h, chat, rx, items).await;
        let replies = events.iter().filter_map(Event::said).count();
        assert_eq!(replies, 2, "{events:?}");
        assert_eq!(model.requests().len(), 2);

        // An album that closes while the user's turn runs gets BUSY.
        let _running = Busy::claim(&h.app.busy, 22).unwrap();
        let (chat, rx) = album_chat(1);
        let events = album(&h, chat, rx, vec![album_item(22, "a3", 0, None)]).await;
        assert_eq!(events, [said(BUSY)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_item_arriving_during_the_wait_restarts_it() {
        let (h, model) = watched(vec![MockTurn::text("both")]);
        let (chat, mut rx) = album_chat(2);
        h.app
            .handle(chat.clone(), album_item(26, "late", 0, None))
            .await;
        // Halfway through the wait: the collector is asleep with the first
        // item's time, wakes, finds a newer one and waits again.
        tokio::time::sleep(h.app.album_wait / 2).await;
        h.app
            .handle(chat.clone(), album_item(26, "late", 1, None))
            .await;
        h.app.finish().await;

        assert_eq!(model.requests().len(), 1);
        let text = model.requests()[0]
            .chat_history
            .last()
            .unwrap()
            .rag_text()
            .unwrap();
        assert_eq!(text.matches("[The user attached").count(), 2);
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert_eq!(events, [Event::Typing, said("both")]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_album_past_telegrams_limit_keeps_its_first_items() {
        let (h, model) = watched(vec![MockTurn::text("ten")]);
        let (chat, rx) = album_chat(ALBUM_LIMIT + 1);
        let items = (0..=ALBUM_LIMIT)
            .map(|n| album_item(23, "big", n, None))
            .collect();
        album(&h, chat, rx, items).await;
        let text = model.requests()[0]
            .chat_history
            .last()
            .unwrap()
            .rag_text()
            .unwrap();
        assert_eq!(text.matches("[The user attached").count(), ALBUM_LIMIT);
        assert_eq!(
            h.logged(),
            ["telegram user 23: an album has more than 10 items; ignoring the rest"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_storage_failure_when_an_album_closes_is_answered() {
        let h = harness(vec![]);
        h.store
            .db_for_tests()
            .execute_batch("DROP TABLE selected_sessions")
            .unwrap();
        let (chat, rx) = album_chat(1);
        let events = album(&h, chat, rx, vec![album_item(24, "a", 0, None)]).await;
        assert_eq!(events, [said(FAILED)]);
        let first = h.logged().remove(0);
        assert!(first.starts_with("telegram user 24: storage: "), "{first}");
        assert!(!lock(&h.app.busy).contains(&24));
    }

    // ---- scheduled jobs ----

    use crate::reminders::{Create, TASK_RUNS_PER_DAY};
    use crate::scheduler::Execute;
    use crate::store::Due;
    use jiff::{SignedDuration, Timestamp};
    use jobs::{Jobs, notice, permanent, task_prompt};

    /// Saturday 2026-10-10 03:00 UTC, 08:30 in Kolkata.
    fn t0() -> Timestamp {
        "2026-10-10T03:00:00Z".parse().unwrap()
    }

    /// Schedule a reminder for Telegram user `user` and claim it `late`
    /// past its time.
    fn due<R: Run + 'static>(
        h: &Harness<R>,
        user: u64,
        args: serde_json::Value,
        late: i64,
    ) -> (Due, Timestamp) {
        let owner = h.store.user(TRANSPORT, &user.to_string()).unwrap().id();
        let args: Create = serde_json::from_value(args).unwrap();
        schedule(&h.store, owner, &args);
        let now = t0() + SignedDuration::from_mins(5 + late);
        let mut claimed = h.store.claim_jobs(now, 1).unwrap();
        (claimed.remove(0), now)
    }

    /// Create a reminder at [`t0`]; a task is confirmed as its user would,
    /// with its id and code. Returns its id and, for a task, its code.
    fn schedule(store: &Store, owner: i64, args: &Create) -> (i64, Option<String>) {
        let shown = store.create_reminder(owner, "s", args, t0()).unwrap();
        let id = shown["id"].as_i64().unwrap();
        let code = shown["confirmation_code"].as_str().map(str::to_string);
        if let Some(code) = &code {
            let said = format!("confirm #{id} {code}");
            store
                .confirm_reminder(owner, "s", id, code, &said, t0())
                .unwrap();
        }
        (id, code)
    }

    fn note(text: &str) -> serde_json::Value {
        serde_json::json!({"kind": "notify", "text": text, "in_minutes": 5})
    }

    fn task(text: &str) -> serde_json::Value {
        serde_json::json!({"kind": "agent_task", "text": text, "in_minutes": 5})
    }

    /// Run `job` with every chat recording to `chat`; return what it sent.
    async fn execute<R: Run + 'static>(
        h: &Harness<R>,
        chat: Recorder,
        mut rx: mpsc::UnboundedReceiver<Event>,
        job: Due,
        now: Timestamp,
    ) -> Vec<Event> {
        let jobs = Jobs::new(h.app.clone(), move |_| chat.clone());
        jobs.execute(job, now).await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    /// The job's (status, sent_at, last_error, attempts).
    fn job_row<R>(h: &Harness<R>, id: i64) -> (String, Option<i64>, Option<String>, i64) {
        h.store
            .db_for_tests()
            .query_row(
                "SELECT status, sent_at, last_error, attempts FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    #[test]
    fn only_refusals_for_good_are_permanent() {
        let api = |e: ApiError| anyhow::Error::from(RequestError::Api(e));
        assert!(permanent(&api(ApiError::BotBlocked)));
        assert!(permanent(&api(ApiError::UserDeactivated)));
        assert!(permanent(&api(ApiError::ChatNotFound)));
        assert!(permanent(&api(ApiError::CantInitiateConversation)));
        let forbidden = "Forbidden: bot can't initiate conversation with a user";
        assert!(permanent(&api(ApiError::Unknown(forbidden.into()))));
        assert!(!permanent(&api(ApiError::Unknown("Bad Request: x".into()))));
        assert!(!permanent(&api(ApiError::MessageIsTooLong)));
        assert!(!permanent(&anyhow::Error::from(RequestError::RetryAfter(
            teloxide::types::Seconds::from_seconds(1)
        ))));
        assert!(!permanent(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn notices_and_task_prompts_say_when_they_are_late() {
        assert_eq!(notice("tea", None), "Reminder: tea");
        assert_eq!(
            notice("tea", Some("Sat 10 Oct 09:00")),
            "Reminder (late: it was due Sat 10 Oct 09:00): tea"
        );
        let on_time = task_prompt(3, "check", "Sat 10 Oct 09:00", false);
        assert!(on_time.starts_with("Scheduled task #3, which the user confirmed earlier, was due Sat 10 Oct 09:00. Do it now"));
        assert!(on_time.ends_with("\n\ncheck"));
        assert!(task_prompt(3, "check", "x", true).contains("was due x, and it is running late."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reminder_is_sent_and_recorded_or_noted_late() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 51, note("stand up"), 0);
        let (chat, rx) = recorder();
        let events = execute(&h, chat, rx, job, now).await;
        assert_eq!(events, [said("Reminder: stand up")]);
        assert_eq!(
            job_row(&h, 1),
            ("done".into(), Some(now.as_millisecond()), None, 0)
        );

        // Ten minutes late: 08:35 in Kolkata was its time.
        let (job, now) = due(&h, 51, note("drink water"), 10);
        let (chat, rx) = recorder();
        let events = execute(&h, chat, rx, job, now).await;
        assert_eq!(
            events,
            [said(
                "Reminder (late: it was due Sat 10 Oct 08:35): drink water"
            )]
        );
        assert!(h.logged().is_empty(), "{:?}", h.logged());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_blocked_bot_fails_the_reminder_and_other_errors_retry() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 52, note("a"), 0);
        let (mut chat, rx) = recorder();
        chat.blocked = true;
        execute(&h, chat, rx, job, now).await;
        let (status, sent, error, _) = job_row(&h, 1);
        assert_eq!((status.as_str(), sent), ("failed", None));
        assert!(error.unwrap().contains("blocked"));

        let (job, now) = due(&h, 52, note("b"), 0);
        let (mut chat, rx) = recorder();
        chat.fail = true;
        execute(&h, chat, rx, job, now).await;
        assert_eq!(
            job_row(&h, 2),
            ("active".into(), None, Some("blocked by the user".into()), 1)
        );
        let logged = h.logged();
        assert!(logged.iter().all(|m| m.contains("failed")), "{logged:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jobs_that_cannot_be_delivered_fail_with_a_reason() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 53, note("a"), 0);
        let unknown = Due {
            kind: "health_sync".into(),
            ..job.clone()
        };
        let (chat, rx) = recorder();
        assert!(execute(&h, chat, rx, unknown, now).await.is_empty());
        assert_eq!(
            job_row(&h, 1).2.as_deref(),
            Some("this build cannot run jobs of kind `health_sync`")
        );

        let (job, now) = due(&h, 53, note("b"), 0);
        let (chat, rx) = recorder();
        let homeless = Due { chat: None, ..job };
        assert!(execute(&h, chat, rx, homeless, now).await.is_empty());
        assert_eq!(
            job_row(&h, 2).2.as_deref(),
            Some("the user has no Telegram chat to deliver to")
        );

        let (job, now) = due(&h, 53, note("c"), 0);
        h.store
            .db_for_tests()
            .execute(
                "INSERT INTO user_settings VALUES (?1, 'Gone/Away', 0)",
                [job.owner],
            )
            .unwrap();
        let (chat, rx) = recorder();
        assert!(execute(&h, chat, rx, job, now).await.is_empty());
        let (status, _, error, _) = job_row(&h, 3);
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("no longer resolves"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_storage_failure_is_logged() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 54, note("a"), 0);
        h.store
            .db_for_tests()
            .execute_batch("ALTER TABLE jobs RENAME TO gone")
            .unwrap();
        let (chat, rx) = recorder();
        assert_eq!(execute(&h, chat, rx, job, now).await, [said("Reminder: a")]);
        let logged = h.logged();
        assert!(logged[0].starts_with("scheduled job 1: "), "{logged:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_runs_as_a_turn_in_the_current_session_and_is_recorded_first() {
        let h = harness(vec![MockTurn::text("**3** headlines")]);
        h.send(55, "/new news").await;
        let (job, now) = due(
            &h,
            55,
            serde_json::json!({"kind": "agent_task",
            "text": "summarise the news", "repeat": "daily", "time": "08:35"}),
            0,
        );
        let (chat, rx) = recorder();
        let events = execute(&h, chat, rx, job, now).await;
        assert_eq!(events[0], Event::Typing);
        assert!(matches!(&events[1], Event::Formatted(text, _) if text == "3 headlines"));
        assert_eq!(h.sessions(55), [("news".to_string(), 2)]);
        let prompt = h
            .store
            .load(&h.store.selected_session(&h.user(55)).unwrap().unwrap().id)
            .unwrap()[0]
            .rag_text()
            .unwrap();
        assert_eq!(
            prompt,
            task_prompt(1, "summarise the news", "Sat 10 Oct 08:35", false)
        );
        let (status, sent, _, _) = job_row(&h, 1);
        assert_eq!(
            (status.as_str(), sent),
            ("active", Some(now.as_millisecond()))
        );
        assert!(!lock(&h.app.busy).contains(&55));
    }

    /// A scheduled turn has no user message, so nothing in it can confirm a
    /// task, even with the id and code in its own prompt.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scheduled_turn_cannot_confirm_a_task() {
        let model = MockCompletionModel::new([
            MockTurn::tool_call(
                "c",
                "reminder_confirm",
                serde_json::json!({"id": 1, "code": "ABCDEFGH"}),
            ),
            MockTurn::text("tried"),
        ]);
        let scripted = model.clone();
        let h = harness_with(move |s| {
            agent::configure_persistent(
                AgentBuilder::new(scripted).memory(s.memory()),
                None,
                &crate::custom::Custom::default(),
                &crate::mcp::Mcp::none(),
                s.memory().store().clone(),
                None,
            )
        });
        let owner = h.store.user(TRANSPORT, "60").unwrap().id();
        h.store
            .db_for_tests()
            .execute(
                "INSERT INTO jobs (user_id, kind, payload, next_run_at, status, created_at,
                                   updated_at, confirm_code, confirm_session, confirm_expires_at)
                 VALUES (?1, 'agent_task', 'x', ?2, 'pending', 0, 0, 'ABCDEFGH', 's', ?2)",
                rusqlite::params![
                    owner,
                    (t0() + SignedDuration::from_hours(1)).as_millisecond()
                ],
            )
            .unwrap();
        let (job, now) = due(&h, 60, task("confirm #1 ABCDEFGH"), 0);
        let (chat, rx) = recorder();
        execute(&h, chat, rx, job, now).await;

        let request = &model.requests()[1];
        let result = serde_json::to_string(request.chat_history.last().unwrap()).unwrap();
        assert!(result.contains("Not confirmed"), "{result}");
        assert_eq!(job_row(&h, 1).0, "pending");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_waits_while_its_user_is_mid_turn_then_is_skipped() {
        let h = harness(vec![]);
        let _running = Busy::claim(&h.app.busy, 56).unwrap();
        let (job, now) = due(&h, 56, task("a"), 0);
        let (chat, rx) = recorder();
        assert!(execute(&h, chat, rx, job, now).await.is_empty());
        let lease: i64 = h
            .store
            .db_for_tests()
            .query_row("SELECT lease_until FROM jobs WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(lease, (now + SignedDuration::from_mins(1)).as_millisecond());

        let (job, now) = due(&h, 56, task("b"), 31);
        let (chat, rx) = recorder();
        let events = execute(&h, chat, rx, job, now).await;
        // The longest overdue first: the first task, now 31 minutes late.
        assert_eq!(
            events,
            [said(
                "I skipped scheduled task #1: you were in a conversation with me for 30 minutes past its time."
            )]
        );
        let (status, sent, error, _) = job_row(&h, 1);
        assert_eq!((status.as_str(), sent), ("done", None));
        assert!(error.unwrap().starts_with("skipped"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tasks_past_the_daily_limit_are_skipped() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 57, task("a"), 0);
        for _ in 0..TASK_RUNS_PER_DAY {
            h.store
                .db_for_tests()
                .execute(
                    "INSERT INTO jobs (user_id, kind, payload, next_run_at, status, sent_at,
                                       created_at, updated_at)
                     VALUES (?1, 'agent_task', 'x', 0, 'done', ?2, 0, 0)",
                    rusqlite::params![job.owner, now.as_millisecond() - 1000],
                )
                .unwrap();
        }
        let (chat, rx) = recorder();
        let events = execute(&h, chat, rx, job, now).await;
        assert_eq!(
            events,
            [said(
                "I skipped scheduled task #1: you can have at most 20 scheduled tasks run in 24 hours."
            )]
        );
        assert_eq!(job_row(&h, 1).0, "done");
        assert_eq!(job_row(&h, 1).1, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_whose_reply_cannot_reach_the_user_stops() {
        let h = harness(vec![MockTurn::text("done")]);
        let (job, now) = due(
            &h,
            58,
            serde_json::json!({"kind": "agent_task",
            "text": "x", "repeat": "daily", "time": "08:35"}),
            0,
        );
        let (mut chat, rx) = recorder();
        chat.blocked = true;
        execute(&h, chat, rx, job, now).await;
        let (status, sent, error, _) = job_row(&h, 1);
        assert_eq!(
            (status.as_str(), sent),
            ("failed", Some(now.as_millisecond()))
        );
        assert!(error.unwrap().contains("blocked"));

        // Any other failure to send the reply leaves the task to run again.
        let h = harness(vec![MockTurn::text("done")]);
        let (job, now) = due(
            &h,
            58,
            serde_json::json!({"kind": "agent_task",
            "text": "x", "repeat": "daily", "time": "08:35"}),
            0,
        );
        let (mut chat, rx) = recorder();
        chat.fail = true;
        execute(&h, chat, rx, job, now).await;
        assert_eq!(job_row(&h, 1).0, "active");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_that_cannot_find_its_session_is_logged() {
        let h = harness(vec![]);
        let (job, now) = due(&h, 59, task("a"), 0);
        h.store
            .db_for_tests()
            .execute_batch("DROP TABLE selected_sessions")
            .unwrap();
        let (chat, rx) = recorder();
        assert!(execute(&h, chat, rx, job, now).await.is_empty());
        let logged = h.logged();
        assert!(
            logged[0].starts_with("scheduled job 1: storage: "),
            "{logged:?}"
        );
    }

    // ---- Google Health ----

    use crate::health::sync::Outcome;
    use crate::health::testing::{CLIENT_SECRET, FakeGoogle, SetClock};
    use crate::telegram::health as tg_health;

    const CODE: &str = "4/SECRET-AUTH-CODE-77";
    /// The fake Google's token values, which must never reach a reply or a log.
    const TOKENS: [&str; 2] = ["rt-1", "at-1"];
    const CALLBACK: &str = "http://127.0.0.1:8080/integrations/google-health/callback";

    struct Fit {
        h: Harness<Agent>,
        model: MockCompletionModel,
        fake: FakeGoogle,
        clock: Arc<SetClock>,
        health: Arc<Health>,
    }

    async fn fit(turns: Vec<MockTurn>) -> Fit {
        let store = Store::open_in_memory().unwrap();
        let fake = FakeGoogle::start().await;
        let clock = SetClock::at("2026-10-10T10:00:00Z");
        let health = crate::health::testing::health(&store, &fake, clock.clone());
        let model = MockCompletionModel::new(turns);
        let given = model.clone();
        let h = harness_over(
            store,
            move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())),
            Some(health.clone()),
        );
        Fit {
            h,
            model,
            fake,
            clock,
            health,
        }
    }

    /// The consent link in a `/connect_health` reply, and its `state`.
    fn link_in(reply: &str) -> (String, String) {
        let link = reply
            .split_whitespace()
            .find(|w| w.starts_with("https://accounts.google.com/"))
            .expect("a link");
        let url = url::Url::parse(link).unwrap();
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned();
        (link.to_string(), state)
    }

    fn callback_url(state: &str) -> String {
        let code = CODE.replace('/', "%2F");
        format!("{CALLBACK}?state={state}&code={code}&scope=sleep")
    }

    /// Nothing sent back holds the pasted code, the client secret or a token.
    fn assert_no_secrets_in(events: &[Event]) {
        let shown = format!("{events:?}");
        for secret in [CODE, CLIENT_SECRET].into_iter().chain(TOKENS) {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    impl Fit {
        /// Nothing the user pasted reached the model, the transcript or a log.
        fn assert_code_stayed_private(&self) {
            assert!(self.model.requests().is_empty(), "the model was called");
            let stored: i64 = self
                .h
                .store
                .db_for_tests()
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap();
            assert_eq!(stored, 0, "something was saved in a transcript");
            let logs = self.h.logged().join("\n");
            for secret in [CODE, "SECRET-AUTH-CODE", "state=", CLIENT_SECRET]
                .into_iter()
                .chain(TOKENS)
            {
                assert!(!logs.contains(secret), "{logs}");
            }
        }

        async fn connect(&self, user: u64) -> String {
            let reply = self.h.reply(user, "/connect_health").await;
            let (_, state) = link_in(&reply);
            let events = self.h.send(user, &callback_url(&state)).await;
            assert_eq!(events[0], said(tg_health::CONNECTED), "{events:?}");
            state
        }
    }

    #[test]
    fn the_health_commands_parse_and_are_in_the_menu() {
        assert_eq!(parse("/connect_health"), Input::ConnectHealth);
        assert_eq!(parse("/connect_health@athena_bot"), Input::ConnectHealth);
        assert_eq!(parse("/disconnect_health"), Input::DisconnectHealth);
        let names: Vec<&str> = COMMANDS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"connect_health") && names.contains(&"disconnect_health"));
        assert!(help("default").contains("/connect_health - "));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_google_health_the_commands_say_so_and_a_pasted_url_is_still_not_a_prompt() {
        let (h, model) = watched(vec![MockTurn::text("must not be asked")]);
        assert_eq!(h.reply(1, "/connect_health").await, tg_health::NOT_SET_UP);
        assert_eq!(
            h.reply(1, "/disconnect_health").await,
            tg_health::NOT_SET_UP
        );
        let pasted = h.reply(1, &callback_url("whatever")).await;
        assert_eq!(pasted, tg_health::NOT_SET_UP);
        // Even a URL on some other host with a code and a state.
        let other = h
            .reply(1, "https://example.com/cb?code=abc&state=def")
            .await;
        assert_eq!(other, tg_health::NOT_SET_UP);
        assert!(model.requests().is_empty());
        let stored: i64 = h
            .store
            .db_for_tests()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_health_sends_a_consent_link_and_stores_only_its_hash() {
        let f = fit(vec![]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (link, state) = link_in(&reply);
        assert!(reply.contains("10 minutes"), "{reply}");
        assert!(reply.contains(CALLBACK), "{reply}");
        assert!(reply.contains("never pass it to the AI"), "{reply}");
        assert!(link.contains("access_type=offline"));
        assert!(!link.contains("writeonly"));
        let owner = f.h.user(1).id();
        let hash = crate::health::hash_state(&state);
        assert!(
            f.h.store
                .health_state_consume(owner, &hash, f.health.now())
                .unwrap()
        );
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pasted_callback_connects_and_syncs_without_the_model_or_a_transcript() {
        let f = fit(vec![MockTurn::text("must not be asked")]).await;
        f.fake.answer(
            "steps",
            200,
            r#"{"dataPoints":[{"steps":{"interval":{"startTime":"2026-10-09T08:00:00Z"},"count":"42"}}]}"#,
        );
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        // Pasted with words around it, as people do.
        let text = format!("ok here: {} thanks", callback_url(&state));
        let events = f.h.send(1, &text).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0], said(tg_health::CONNECTED));
        assert_no_secrets_in(&events);
        let synced = events[1].said().unwrap();
        let counted = synced.starts_with("Synced 1 days of the last 14");
        assert!(counted, "{synced}");

        let exchange = &f.fake.seen_at("/token")[0];
        assert_eq!(exchange.form["code"], CODE);
        let owner = f.h.user(1).id();
        let conn = f.h.store.health_connection(owner).unwrap().unwrap();
        assert_eq!(conn.status, "connected");
        assert!(conn.last_synced_at.is_some());
        assert_eq!(
            f.h.store
                .health_days(owner, "2026-10-01", "2026-10-31")
                .unwrap()
                .len(),
            1
        );
        f.assert_code_stayed_private();
        // The same URL again is just a spent link.
        let again = f.h.reply(1, &text).await;
        assert_eq!(again, tg_health::BAD_STATE);
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pasted_callback_in_a_files_caption_is_not_a_prompt_either() {
        let f = fit(vec![MockTurn::text("must not be asked")]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        let (chat, rx) = recorder();
        let incoming = with_file(1, Some(&callback_url(&state)), photo("p", 10));
        let events = exchange(&f.h, chat, rx, incoming).await;
        assert_eq!(events[0], said(tg_health::CONNECTED));
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_for_someone_else_a_wrong_or_an_expired_state_connects_nothing() {
        let f = fit(vec![MockTurn::text("must not be asked")]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        // Another user pasting it.
        assert_eq!(
            f.h.reply(2, &callback_url(&state)).await,
            tg_health::BAD_STATE
        );
        // A made-up state, and a callback with no state at all.
        assert_eq!(
            f.h.reply(1, &callback_url("forged")).await,
            tg_health::BAD_STATE
        );
        let bare = format!("{CALLBACK}?code=abc");
        assert_eq!(f.h.reply(1, &bare).await, tg_health::BAD_STATE);
        // After the ten minutes.
        f.clock.set("2026-10-10T10:10:00Z");
        assert_eq!(
            f.h.reply(1, &callback_url(&state)).await,
            tg_health::BAD_STATE
        );
        assert!(f.fake.seen().is_empty());
        for user in [1, 2] {
            let owner = f.h.user(user).id();
            assert!(f.h.store.health_connection(owner).unwrap().is_none());
        }
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_users_attempt_does_not_spend_the_link() {
        let f = fit(vec![MockTurn::text("must not be asked")]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        assert_eq!(
            f.h.reply(2, &callback_url(&state)).await,
            tg_health::BAD_STATE
        );
        // The owner's paste still works with the same link.
        let events = f.h.send(1, &callback_url(&state)).await;
        assert_eq!(events[0], said(tg_health::CONNECTED), "{events:?}");
        let (owner, stranger) = (f.h.user(1).id(), f.h.user(2).id());
        assert!(f.h.store.health_connection(owner).unwrap().is_some());
        assert!(f.h.store.health_connection(stranger).unwrap().is_none());
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_is_good_up_to_its_last_millisecond() {
        let f = fit(vec![]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        f.clock.set("2026-10-10T10:09:59.999Z");
        let events = f.h.send(1, &callback_url(&state)).await;
        assert_eq!(events[0], said(tg_health::CONNECTED), "{events:?}");
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declining_a_missing_code_and_a_refused_code_each_get_a_clear_reply() {
        let f = fit(vec![MockTurn::text("must not be asked")]).await;
        let state_of = |reply: &str| link_in(reply).1;
        let reply = f.h.reply(1, "/connect_health").await;
        let denied = format!("{CALLBACK}?error=access_denied&state={}", state_of(&reply));
        assert_eq!(f.h.reply(1, &denied).await, tg_health::DENIED);
        let reply = f.h.reply(1, "/connect_health").await;
        let no_code = format!("{CALLBACK}?state={}", state_of(&reply));
        assert_eq!(f.h.reply(1, &no_code).await, tg_health::NO_CODE);
        let reply = f.h.reply(1, "/connect_health").await;
        f.fake.answer(
            "token",
            400,
            r#"{"error":"invalid_request","error_description":"SECRET-AUTH-CODE"}"#,
        );
        let refused = f.h.reply(1, &callback_url(&state_of(&reply))).await;
        let said_so = refused.starts_with("I could not connect Google Health");
        assert!(said_so, "{refused}");
        assert!(refused.contains("invalid_request"), "{refused}");
        assert!(!refused.contains("SECRET"), "{refused}");
        assert!(!refused.contains(CLIENT_SECRET), "{refused}");
        assert!(
            f.h.logged()
                .iter()
                .any(|l| l.contains("connecting Google Health failed"))
        );
        f.assert_code_stayed_private();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_database_failure_while_connecting_is_answered_and_logged() {
        let f = fit(vec![]).await;
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        f.h.store
            .db_for_tests()
            .execute_batch("DROP TABLE health_oauth_states")
            .unwrap();
        let events = f.h.send(1, &callback_url(&state)).await;
        assert!(
            events[0]
                .said()
                .unwrap()
                .starts_with("I could not connect Google Health")
        );
        f.assert_code_stayed_private();
        // And `/connect_health` itself fails like any storage error.
        let again = f.h.reply(1, "/connect_health").await;
        assert_eq!(again, FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_first_sync_is_logged_and_told() {
        let f = fit(vec![]).await;
        f.fake.answer("sleep", 500, "{}");
        let reply = f.h.reply(1, "/connect_health").await;
        let (_, state) = link_in(&reply);
        let events = f.h.send(1, &callback_url(&state)).await;
        let told = events[1].said().unwrap();
        let failed = told.contains("first sync failed (Google answered HTTP 500)");
        assert!(failed, "{told}");
        assert!(
            f.h.logged()
                .iter()
                .any(|l| l.contains("first Google Health sync"))
        );
    }

    #[test]
    fn the_first_sync_says_what_happened() {
        let said = |o: Outcome| tg_health::first_sync_message(&o);
        assert!(
            said(Outcome::Synced {
                days: 3,
                unavailable: vec![]
            })
            .starts_with("Synced 3 days")
        );
        let some = said(Outcome::Synced {
            days: 3,
            unavailable: vec!["sleep".into(), "weight".into()],
        });
        assert!(some.contains("did not allow: sleep, weight"), "{some}");
        assert_eq!(said(Outcome::Revoked), tg_health::REVOKED);
        assert_eq!(said(Outcome::AlreadyRevoked), tg_health::REVOKED);
        assert!(said(Outcome::Failed("x".into())).contains("(x)"));
        for other in [
            Outcome::NotConnected,
            Outcome::Running,
            Outcome::Cooldown("2026-10-10T11:00:00Z".parse().unwrap()),
        ] {
            assert!(said(other).contains("did not start"));
        }
    }

    #[test]
    fn disconnecting_is_explained_in_every_case() {
        use crate::health::sync::Remote;
        let said = |r| tg_health::disconnect_message(r);
        assert_eq!(said(None), "Google Health is not connected.");
        assert!(said(Some(Remote::Revoked)).contains("Google confirmed"));
        assert!(said(Some(Remote::Failed)).contains("myaccount.google.com/permissions"));
        assert!(said(Some(Remote::NotNeeded)).contains("already revoked"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_health_revokes_at_google_and_deletes_the_token() {
        let f = fit(vec![]).await;
        assert_eq!(
            f.h.reply(1, "/disconnect_health").await,
            "Google Health is not connected."
        );
        f.connect(1).await;
        let owner = f.h.user(1).id();
        let reply = f.h.reply(1, "/disconnect_health").await;
        assert!(reply.contains("Google confirmed"), "{reply}");
        assert_eq!(f.fake.seen_at("/revoke")[0].form["token"], "rt-1");
        assert!(f.h.store.health_connection(owner).unwrap().is_none());
        // Google refusing the revoke is said, not hidden.
        f.connect(1).await;
        f.fake.answer("revoke", 400, "{}");
        let reply = f.h.reply(1, "/disconnect_health").await;
        assert!(reply.contains("could not confirm"), "{reply}");
        assert!(f.h.store.health_connection(owner).unwrap().is_none());
    }

    // ---- the daily pass, through the scheduler's system hook ----

    fn collect(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_daily_pass_syncs_quietly_and_tells_a_revoked_user_once() {
        use crate::scheduler::Execute;
        let f = fit(vec![]).await;
        f.connect(1).await;
        let (chat, mut rx) = recorder();
        let jobs = Jobs::new(f.h.app.clone(), move |_| chat.clone());
        // Only the window requests are counted: the daily pass also fetches
        // history chunks, which end at or before the window's start.
        let window_asks = |end: &str| {
            f.fake
                .seen_at("/steps/dataPoints")
                .iter()
                .filter(|s| s.query["filter"].contains(&format!(r#"start_time < "{end}""#)))
                .count()
        };
        // Connecting synced already: at 15:30 in Kolkata nothing is due.
        jobs.system(f.health.now()).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(f.fake.seen_at("/steps/dataPoints").len(), 1);
        // The next morning it is, and nothing is said when it works.
        f.clock.set("2026-10-11T00:00:00Z");
        jobs.system(f.health.now()).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(window_asks("2026-10-11T18:30:00Z"), 1);
        // The morning after, Google has revoked the access.
        f.clock.set("2026-10-12T00:00:00Z");
        f.fake.answer("token", 400, r#"{"error":"invalid_grant"}"#);
        jobs.system(f.health.now()).await;
        assert_eq!(collect(&mut rx), [said(tg_health::REVOKED)]);
        // Revoked users are not tried again, and not told again.
        f.clock.set("2026-10-13T00:00:00Z");
        jobs.system(f.health.now()).await;
        assert!(collect(&mut rx).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_daily_sync_is_logged_and_not_said() {
        use crate::scheduler::Execute;
        let f = fit(vec![]).await;
        f.connect(1).await;
        f.clock.set("2026-10-11T00:00:00Z");
        f.fake.answer("sleep", 500, "{}");
        let (chat, mut rx) = recorder();
        Jobs::new(f.h.app.clone(), move |_| chat.clone())
            .system(f.health.now())
            .await;
        assert!(collect(&mut rx).is_empty());
        assert!(
            f.h.logged()
                .iter()
                .any(|l| l.contains("Google Health sync for user") && l.contains("HTTP 500")),
            "{:?}",
            f.h.logged()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_daily_pass_logs_a_failure_to_list_users_and_does_nothing_without_health() {
        use crate::scheduler::Execute;
        let f = fit(vec![]).await;
        f.h.store
            .db_for_tests()
            .execute_batch("DROP TABLE health_connections")
            .unwrap();
        let (chat, mut rx) = recorder();
        let chat2 = chat.clone();
        Jobs::new(f.h.app.clone(), move |_| chat.clone())
            .system(f.health.now())
            .await;
        assert!(f.h.logged()[0].starts_with("Google Health daily sync failed"));
        // No Google Health at all: nothing to do, nothing logged.
        let plain = harness(vec![]);
        Jobs::new(plain.app.clone(), move |_| chat2.clone())
            .system(f.health.now())
            .await;
        assert!(plain.logged().is_empty());
        assert!(collect(&mut rx).is_empty());
    }

    // ---- the daily training brief ----

    use crate::store::BriefCandidate;

    /// 06:30 in Kolkata on 2026-10-10 is 01:00Z: the brief's due instant.
    const DUE: &str = "2026-10-10T01:00:00Z";
    const NEXT_DAY: &str = "2026-10-11T01:00:00Z";

    fn when(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn brief_tasks() -> Semaphore {
        Semaphore::new(jobs::TASKS)
    }

    /// One daily pass at `now`, every chat recording to `chat`.
    async fn brief_pass<R: Run + 'static>(
        h: &Harness<R>,
        chat: &Recorder,
        tasks: &Semaphore,
        now: Timestamp,
    ) {
        let chat = chat.clone();
        h.app
            .daily_brief(&move |_: i64| chat.clone(), tasks, now)
            .await;
    }

    /// A brief at `time` for Telegram user `id`, not yet sent on any day.
    fn set_brief<R: Run + 'static>(h: &Harness<R>, id: u64, time: &str) -> i64 {
        let owner = h.user(id).id();
        h.store
            .brief_set(
                owner,
                time,
                "2026-10-09",
                false,
                when("2026-10-09T12:00:00Z"),
            )
            .unwrap();
        owner
    }

    fn brief_row<R: Run + 'static>(h: &Harness<R>, owner: i64) -> crate::store::Brief {
        h.store.brief(owner).unwrap().unwrap()
    }

    /// The brief's reply, as the chat sees it: typing, then the text.
    fn brief_sent(text: &str) -> Vec<Event> {
        vec![Event::Typing, said(text)]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_due_brief_is_one_scheduled_turn_in_the_selected_session() {
        let model = MockCompletionModel::new([MockTurn::text("Push day, 2.5 kg up.")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let user = h.user(42);
        let owner = set_brief(&h, 42, "06:30");
        let side = h.store.open_session(&user, "side").unwrap();
        assert!(h.store.select_session(&user, &side.id).unwrap());

        let (chat, mut rx) = recorder();
        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;

        assert_eq!(collect(&mut rx), brief_sent("Push day, 2.5 kg up."));
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        let asked = serde_json::to_string(requests[0].chat_history.last().unwrap()).unwrap();
        assert!(asked.contains("daily training brief for 2026-10-10 (Saturday)"));
        // The turn ran in the selected session, not in a new default one.
        assert_eq!(h.sessions(42), [("side".to_string(), 2)]);
        let row = brief_row(&h, owner);
        assert_eq!(row.last_sent_date.as_deref(), Some("2026-10-10"));
        assert_eq!(row.last_attempt_at, Some(when(DUE)));
        assert_eq!(row.last_error, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_day_is_sent_once_even_when_passes_overlap() {
        let model = MockCompletionModel::new([MockTurn::text("one"), MockTurn::text("two")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");

        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();
        let (a, b) = (
            brief_pass(&h, &chat, &tasks, when(DUE)),
            brief_pass(&h, &chat, &tasks, when(DUE)),
        );
        tokio::join!(a, b);
        assert_eq!(collect(&mut rx), brief_sent("one"));
        assert_eq!(model.requests().len(), 1);

        // Later the same day, and a new Telegram on the same store: nothing.
        brief_pass(&h, &chat, &tasks, when("2026-10-10T02:00:00Z")).await;
        let restarted_model = MockCompletionModel::new([MockTurn::text("unused")]);
        let given = restarted_model.clone();
        let restarted = harness_over(
            h.store.clone(),
            move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())),
            None,
        );
        brief_pass(&restarted, &chat, &tasks, when("2026-10-10T02:30:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        assert!(restarted_model.requests().is_empty());

        // The next morning it is sent again.
        brief_pass(&h, &chat, &tasks, when(NEXT_DAY)).await;
        assert_eq!(collect(&mut rx), brief_sent("two"));
        assert_eq!(model.requests().len(), 2);
        assert_eq!(
            brief_row(&h, owner).last_sent_date.as_deref(),
            Some("2026-10-11")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_happens_before_the_time_or_after_the_window() {
        let h = harness(vec![MockTurn::text("never")]);
        let owner = set_brief(&h, 42, "06:30");
        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();

        brief_pass(&h, &chat, &tasks, when("2026-10-10T00:59:59Z")).await;
        assert!(collect(&mut rx).is_empty());
        // Four hours after the time the day is missed, and nothing records it.
        brief_pass(&h, &chat, &tasks, when("2026-10-10T05:00:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        let row = brief_row(&h, owner);
        assert_eq!((row.last_sent_date, row.last_attempt_at), (None, None));
        assert!(h.logged().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_zone_moves_a_brief_that_has_not_yet_come() {
        let model = MockCompletionModel::new([MockTurn::text("Dubai morning")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();

        // 06:20 in Kolkata: not yet.
        brief_pass(&h, &chat, &tasks, when("2026-10-10T00:50:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        // The user moves to Dubai (UTC+4): 06:30 there is 02:30Z.
        h.store.set_timezone(owner, "Asia/Dubai").unwrap();
        brief_pass(&h, &chat, &tasks, when("2026-10-10T01:30:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        brief_pass(&h, &chat, &tasks, when("2026-10-10T02:30:00Z")).await;
        assert_eq!(collect(&mut rx), brief_sent("Dubai morning"));
        brief_pass(&h, &chat, &tasks, when("2026-10-10T02:31:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_york_sends_once_a_local_day_across_both_2026_clock_changes() {
        let ny = crate::timezone::parse("America/New_York").unwrap();
        let days = [
            "2026-03-07",
            "2026-03-08",
            "2026-03-09",
            "2026-10-31",
            "2026-11-01",
            "2026-11-02",
        ];
        let model = MockCompletionModel::new(days.map(|d| MockTurn::text(format!("brief {d}"))));
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        h.store.set_timezone(owner, "America/New_York").unwrap();
        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();

        let mut sent = Vec::new();
        for day in days {
            let start = day
                .parse::<jiff::civil::Date>()
                .unwrap()
                .at(0, 0, 0, 0)
                .to_zoned(ny.clone())
                .unwrap()
                .timestamp();
            // Every half hour from local midnight to just past 13:00 on the day.
            for step in 0..28 {
                let now = start + jiff::SignedDuration::from_mins(30 * step);
                brief_pass(&h, &chat, &tasks, now).await;
                if collect(&mut rx).iter().any(|e| e.said().is_some()) {
                    sent.push(now.to_zoned(ny.clone()).strftime("%F %H:%M").to_string());
                }
            }
        }
        assert_eq!(
            sent,
            [
                "2026-03-07 06:30",
                "2026-03-08 06:30",
                "2026-03-09 06:30",
                "2026-10-31 06:30",
                "2026-11-01 06:30",
                "2026-11-02 06:30",
            ]
        );
        assert_eq!(model.requests().len(), 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_brief_waits_for_a_user_mid_turn_and_is_skipped_past_its_window() {
        let model = MockCompletionModel::new([MockTurn::text("after the turn")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();

        // Mid-turn at the due time: nothing is claimed, so nothing is lost.
        let busy = Busy::claim(&h.app.busy, 42).unwrap();
        brief_pass(&h, &chat, &tasks, when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(brief_row(&h, owner).last_sent_date, None);
        drop(busy);
        // Free an hour later, still in the window: sent then.
        brief_pass(&h, &chat, &tasks, when("2026-10-10T02:00:00Z")).await;
        assert_eq!(collect(&mut rx), brief_sent("after the turn"));

        // Another user, mid-turn until the window closes: skipped, not made up.
        let late = set_brief(&h, 43, "06:30");
        let busy = Busy::claim(&h.app.busy, 43).unwrap();
        brief_pass(&h, &chat, &tasks, when("2026-10-10T04:59:00Z")).await;
        drop(busy);
        brief_pass(&h, &chat, &tasks, when("2026-10-10T05:00:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(brief_row(&h, late).last_sent_date, None);
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_permit_means_nothing_now_and_the_brief_later_in_the_window() {
        let model = MockCompletionModel::new([MockTurn::text("with a permit")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        let (chat, mut rx) = recorder();

        brief_pass(&h, &chat, &Semaphore::new(0), when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(brief_row(&h, owner).last_sent_date, None);
        brief_pass(&h, &chat, &brief_tasks(), when("2026-10-10T01:30:00Z")).await;
        assert_eq!(collect(&mut rx), brief_sent("with a permit"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_switched_off_brief_or_one_without_telegram_is_never_a_candidate() {
        let h = harness(vec![MockTurn::text("never")]);
        let off = set_brief(&h, 42, "06:30");
        h.store.brief_off(off, when(DUE)).unwrap();
        let cli = h.store.user("cli", "local").unwrap().id();
        h.store
            .brief_set(cli, "06:30", "2026-10-09", false, when(DUE))
            .unwrap();
        let (chat, mut rx) = recorder();
        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        assert!(h.logged().is_empty());
        assert_eq!(brief_row(&h, cli).last_sent_date, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_brief_waits_for_todays_health_sync_then_goes_without_it() {
        let f = fit(vec![MockTurn::text("Brief without today's sync")]).await;
        f.connect(1).await;
        let owner = set_brief(&f.h, 1, "06:30");
        let (chat, mut rx) = recorder();
        let tasks = brief_tasks();

        // Connected yesterday's morning: at 06:30 the sync is still to come.
        brief_pass(&f.h, &chat, &tasks, when(NEXT_DAY)).await;
        assert!(collect(&mut rx).is_empty());
        brief_pass(&f.h, &chat, &tasks, when("2026-10-11T01:29:59Z")).await;
        assert!(collect(&mut rx).is_empty());
        // Half an hour after the time, it is sent without the sync.
        brief_pass(&f.h, &chat, &tasks, when("2026-10-11T01:30:00Z")).await;
        assert_eq!(collect(&mut rx), brief_sent("Brief without today's sync"));
        assert_eq!(
            brief_row(&f.h, owner).last_sent_date,
            Some("2026-10-11".into())
        );
        // Waiting is the brief's own: the health sync was not run by it.
        assert_eq!(f.fake.seen_at("/steps/dataPoints").len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_daily_pass_syncs_health_before_a_brief_is_due() {
        let f = fit(vec![MockTurn::text("Brief after today's sync")]).await;
        f.connect(1).await;
        set_brief(&f.h, 1, "06:30");
        f.clock.set(NEXT_DAY);
        let (chat, mut rx) = recorder();
        let scheduler = crate::scheduler::Scheduler::new(
            f.h.store.clone(),
            jobs::Jobs::new(f.h.app.clone(), move |_: i64| chat.clone()),
            f.h.app.log.clone(),
        )
        .clock(f.clock.clone());
        let mut running = JoinSet::new();
        scheduler.tick(&mut running).await.unwrap();
        while running.join_next().await.is_some() {}

        // Synced at 06:30, so no wait: the brief goes at its time.
        assert_eq!(collect(&mut rx), brief_sent("Brief after today's sync"));
        // Today's window was asked for once; the history chunks that follow
        // in the same pass end before the window starts.
        let today_window = f
            .fake
            .seen_at("/steps/dataPoints")
            .into_iter()
            .filter(|s| s.query["filter"].contains(r#"start_time < "2026-10-11T18:30:00Z""#))
            .count();
        assert_eq!(today_window, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_connection_or_once_revoked_the_brief_is_sent_on_time() {
        let never = fit(vec![MockTurn::text("no connection")]).await;
        set_brief(&never.h, 1, "06:30");
        let (chat, mut rx) = recorder();
        brief_pass(&never.h, &chat, &brief_tasks(), when(DUE)).await;
        assert_eq!(collect(&mut rx), brief_sent("no connection"));

        let revoked = fit(vec![MockTurn::text("revoked")]).await;
        revoked.connect(1).await;
        let owner = set_brief(&revoked.h, 1, "06:30");
        revoked
            .h
            .store
            .health_mark_revoked(owner, "Google revoked it", when(DUE))
            .unwrap();
        let (chat, mut rx) = recorder();
        brief_pass(&revoked.h, &chat, &brief_tasks(), when(DUE)).await;
        assert_eq!(collect(&mut rx), brief_sent("revoked"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_that_blocked_the_bot_switches_the_brief_off_for_good() {
        let model = MockCompletionModel::new([MockTurn::text("blocked")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        let (mut chat, mut rx) = recorder();
        chat.blocked = true;
        let tasks = brief_tasks();

        brief_pass(&h, &chat, &tasks, when(DUE)).await;
        assert_eq!(collect(&mut rx), brief_sent("blocked"));
        let row = brief_row(&h, owner);
        assert!(!row.enabled);
        assert!(
            row.last_error
                .as_deref()
                .unwrap_or_default()
                .contains("refused")
        );
        assert_eq!(row.last_sent_date.as_deref(), Some("2026-10-10"));

        brief_pass(&h, &chat, &tasks, when(NEXT_DAY)).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_send_keeps_the_brief_on_and_the_day_claimed() {
        let model = MockCompletionModel::new([MockTurn::text("lost on the way")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = set_brief(&h, 42, "06:30");
        let (mut chat, mut rx) = recorder();
        chat.fail = true;
        let tasks = brief_tasks();

        brief_pass(&h, &chat, &tasks, when(DUE)).await;
        assert_eq!(collect(&mut rx).len(), 2, "typing and the attempt");
        let row = brief_row(&h, owner);
        assert!(row.enabled);
        assert_eq!(row.last_sent_date.as_deref(), Some("2026-10-10"));
        assert!(
            h.logged()
                .iter()
                .any(|l| l.contains("sending a message failed"))
        );

        // Not tried again that day, even inside the window.
        brief_pass(&h, &chat, &tasks, when("2026-10-10T02:00:00Z")).await;
        assert!(collect(&mut rx).is_empty());
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_model_turn_gets_the_standard_reply_and_claims_the_day() {
        let h = harness(vec![MockTurn::error("provider unavailable")]);
        let owner = set_brief(&h, 42, "06:30");
        let (chat, mut rx) = recorder();

        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;
        assert_eq!(
            collect(&mut rx),
            [
                Event::Typing,
                said("The model failed to answer. Try again in a moment.")
            ]
        );
        let row = brief_row(&h, owner);
        assert!(row.enabled);
        assert_eq!(row.last_sent_date.as_deref(), Some("2026-10-10"));
        assert!(h.logged().iter().any(|l| l.contains("turn failed")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_zone_that_no_longer_resolves_is_logged_and_noted_and_left_on() {
        let h = harness(vec![MockTurn::text("never")]);
        let owner = set_brief(&h, 42, "06:30");
        h.store.set_timezone(owner, "UTC").unwrap();
        h.store
            .db_for_tests()
            .execute(
                "UPDATE user_settings SET timezone = 'Gone/Away' WHERE user_id = ?1",
                [owner],
            )
            .unwrap();
        let (chat, mut rx) = recorder();

        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        let row = brief_row(&h, owner);
        assert!(row.enabled);
        assert_eq!(row.last_sent_date, None);
        assert!(
            row.last_error
                .as_deref()
                .unwrap()
                .contains("no longer resolves")
        );
        assert!(
            h.logged()
                .iter()
                .any(|l| l.starts_with(&format!("daily brief for user {owner} failed"))),
            "{:?}",
            h.logged()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failure_to_read_the_briefs_is_logged_and_nothing_is_sent() {
        let h = harness(vec![MockTurn::text("never")]);
        h.store
            .db_for_tests()
            .execute_batch("DROP TABLE daily_briefs")
            .unwrap();
        let (chat, mut rx) = recorder();

        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        assert!(
            h.logged()[0].starts_with("daily brief: reading the briefs failed"),
            "{:?}",
            h.logged()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_brief_turn_cannot_confirm_a_reminder() {
        let model = MockCompletionModel::new([
            MockTurn::tool_call(
                "c",
                "reminder_confirm",
                serde_json::json!({"id": 1, "code": "ABCDEFGH"}),
            ),
            MockTurn::text("tried"),
        ]);
        let scripted = model.clone();
        let h = harness_with(move |s| {
            agent::configure_persistent(
                AgentBuilder::new(scripted).memory(s.memory()),
                None,
                &crate::custom::Custom::default(),
                &crate::mcp::Mcp::none(),
                s.memory().store().clone(),
                None,
            )
        });
        let user = h.user(60);
        let owner = set_brief(&h, 60, "06:30");
        // A pending task whose code the brief's own session could confirm.
        let session = h.store.open_session(&user, DEFAULT_SESSION).unwrap();
        h.store
            .db_for_tests()
            .execute(
                "INSERT INTO jobs (user_id, kind, payload, next_run_at, status, created_at,
                                   updated_at, confirm_code, confirm_session, confirm_expires_at)
                 VALUES (?1, 'agent_task', 'x', ?2, 'pending', 0, 0, 'ABCDEFGH', ?3, ?2)",
                rusqlite::params![
                    owner,
                    (when(DUE) + jiff::SignedDuration::from_hours(1)).as_millisecond(),
                    session.id
                ],
            )
            .unwrap();
        let (chat, _rx) = recorder();
        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;

        let refused =
            serde_json::to_string(model.requests()[1].chat_history.last().unwrap()).unwrap();
        assert!(refused.contains("Not confirmed"), "{refused}");
        assert_eq!(job_row(&h, 1).0, "pending");
    }

    /// A pass that read the candidate before another pass claimed today must
    /// not send a second brief: the claim itself refuses it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_candidate_read_before_today_was_claimed_is_not_sent_again() {
        let model = MockCompletionModel::new([MockTurn::text("never")]);
        let given = model.clone();
        let h =
            harness_with(move |s| agent::configure(AgentBuilder::new(given).memory(s.memory())));
        let owner = h.user(42).id();
        h.store
            .brief_set(owner, "06:30", "2026-10-10", true, when(DUE))
            .unwrap();
        // The candidate as an earlier pass read it, before today was claimed.
        let stale = BriefCandidate {
            owner,
            chat: 42,
            local_time: "06:30".into(),
            last_sent_date: None,
        };
        let (chat, mut rx) = recorder();
        let make = move |_: i64| chat.clone();
        h.app
            .try_brief(&make, &brief_tasks(), stale, when(DUE))
            .await
            .unwrap();
        assert!(collect(&mut rx).is_empty());
        assert!(model.requests().is_empty());
    }

    /// A time that cannot be planned (stored by hand, not by `daily_brief_set`)
    /// is logged and noted, and the brief stays on for the user to fix.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_time_that_cannot_be_planned_is_noted_and_left_on() {
        let h = harness(vec![MockTurn::text("never")]);
        let owner = set_brief(&h, 42, "06:30");
        h.store
            .db_for_tests()
            .execute(
                "UPDATE daily_briefs SET local_time = '24:00' WHERE user_id = ?1",
                [owner],
            )
            .unwrap();
        let (chat, mut rx) = recorder();

        brief_pass(&h, &chat, &brief_tasks(), when(DUE)).await;
        assert!(collect(&mut rx).is_empty());
        let row = brief_row(&h, owner);
        assert!(row.enabled);
        assert!(
            row.last_error
                .as_deref()
                .unwrap_or_default()
                .contains("time must be")
        );
        assert!(h.logged().iter().any(|l| l.contains("failed: ")));
    }
}
