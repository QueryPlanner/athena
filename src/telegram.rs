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

use crate::agent;
use crate::runner::Run;
use crate::service::{self, Service, Session, User};
use crate::shutdown;
use crate::store::{self, Store};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use teloxide::dispatching::{DefaultKey, Dispatcher, ShutdownToken, UpdateFilterExt};
use teloxide::prelude::{Requester, Update};
use teloxide::types::{BotCommand, ChatAction, ChatId, Message};
use teloxide::update_listeners::Polling;
use teloxide::{Bot, RequestError, dptree};
use tokio::task::JoinSet;

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

pub const BUSY: &str =
    "Still working on your last message. Send this one again once I have replied.";
pub const NOT_TEXT: &str = "I only read text messages.";
pub const SWITCH_USAGE: &str = "Usage: /switch NAME. /sessions lists your sessions.";
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
    ("help", "What this bot is and its commands"),
];

/// Receives problems worth an operator's attention. Never message text.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// How the binary logs: to stderr.
pub fn log_to_stderr(message: &str) {
    eprintln!("telegram: {message}");
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
        let bot = Bot::new(&self.token);
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
/// Cuts at the last line break in the second half of the window, else at the
/// last whitespace there, else at the last character that fits: never inside
/// a UTF-8 character, though a hard cut can separate the parts of a
/// multi-codepoint emoji. The line break or space cut at is dropped.
/// Chunks that are only whitespace, which Telegram refuses, are skipped.
/// `limit` must be at least 2, the size of the widest character.
pub fn split(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while utf16_len(rest) > limit {
        let fits = fit(rest, limit);
        let (end, next) = break_at(&rest[..fits], fits / 2).unwrap_or((fits, fits));
        keep(&mut chunks, &rest[..end]);
        rest = &rest[next..];
    }
    keep(&mut chunks, rest);
    chunks
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

/// Where to cut `window`: (end of this chunk, start of the next), at a line
/// break or whitespace no earlier than byte `min`.
fn break_at(window: &str, min: usize) -> Option<(usize, usize)> {
    let usable = |i: usize| i > 0 && i >= min;
    let newline = window
        .rfind('\n')
        .filter(|&i| usable(i))
        .map(|i| (i, i + 1));
    newline.or_else(|| {
        window
            .char_indices()
            .rev()
            .find(|&(i, c)| c.is_whitespace() && usable(i))
            .map(|(i, c)| (i, i + c.len_utf8()))
    })
}

fn keep(chunks: &mut Vec<String>, chunk: &str) {
    if !chunk.trim().is_empty() {
        chunks.push(chunk.to_string());
    }
}

/// The messages a reply is sent as: split to fit, at most [`MAX_CHUNKS`].
pub fn chunks(reply: &str) -> Vec<String> {
    let mut parts = split(reply, MESSAGE_LIMIT);
    if parts.is_empty() {
        return vec!["(The model sent an empty reply.)".into()];
    }
    if parts.len() > MAX_CHUNKS {
        let dropped = parts.len() - (MAX_CHUNKS - 1);
        parts.truncate(MAX_CHUNKS - 1);
        parts.push(format!(
            "(Reply cut short: {dropped} more messages not sent. The whole reply is saved in this session.)"
        ));
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
}

/// A message as the logic needs it, whatever transport library delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub chat_id: i64,
    pub private: bool,
    /// The sender's Telegram user id; absent for channel posts.
    pub user_id: Option<u64>,
    /// Absent for photos, stickers and other non-text messages.
    pub text: Option<String>,
}

/// The bot: turns messages into service calls and replies.
pub struct Telegram<R> {
    service: Arc<Service>,
    /// The same database as `service`, for the session selection, which the
    /// service does not expose yet.
    store: Store,
    agent: Arc<R>,
    log: Log,
    typing_every: Duration,
    /// Users with a turn running.
    busy: Arc<Mutex<HashSet<u64>>>,
    /// Turns running or finished and not yet reaped. [`Telegram::finish`]
    /// waits for them.
    turns: Mutex<JoinSet<()>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
            log,
            typing_every: TYPING_EVERY,
            busy: Arc::default(),
            turns: Mutex::default(),
        }
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
        let reply = match self.respond(&chat, user_id, incoming.text.as_deref()).await {
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
        text: Option<&str>,
    ) -> Result<Option<String>, service::Error> {
        let Some(text) = text else {
            return Ok(Some(NOT_TEXT.into()));
        };
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let reply = match parse(text) {
            Input::Start | Input::Help => help(&self.current(&user).await?.name),
            Input::New(name) => self.create(&user, name).await?,
            Input::Sessions => self.list(&user).await?,
            Input::Switch(None) => SWITCH_USAGE.into(),
            Input::Switch(Some(name)) => self.switch(&user, name).await?,
            Input::Usage => self.usage(&user).await?,
            Input::Unknown(cmd) => format!("Unknown command /{cmd}.\n\n{}", command_list()),
            Input::Text(prompt) => return self.start_turn(chat, user, user_id, prompt).await,
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

    /// Start a turn for `prompt` in the user's current session, unless one
    /// of theirs is already running. Returns what to reply now, if anything.
    async fn start_turn<C: Chat>(
        self: &Arc<Self>,
        chat: &C,
        user: User,
        user_id: u64,
        prompt: &str,
    ) -> Result<Option<String>, service::Error> {
        let Some(busy) = Busy::claim(&self.busy, user_id) else {
            return Ok(Some(BUSY.into()));
        };
        let session = self.current(&user).await?;
        let (app, chat, prompt) = (self.clone(), chat.clone(), prompt.to_string());
        let mut turns = lock(&self.turns);
        while turns.try_join_next().is_some() {}
        turns.spawn(async move {
            let _busy = busy;
            app.turn(chat, user, session, prompt).await;
        });
        Ok(None)
    }

    /// Run one turn and send its reply, showing "typing..." meanwhile.
    ///
    /// The model call runs in its own task: a panic in it becomes a reply,
    /// and nothing cancels it once started.
    async fn turn<C: Chat>(&self, chat: C, user: User, session: Session, prompt: String) {
        typing(&chat, &self.log).await;
        let typing = tokio::spawn(keep_typing(
            chat.clone(),
            self.typing_every,
            self.log.clone(),
        ));
        let (service, agent) = (self.service.clone(), self.agent.clone());
        let who = user.external_id().to_string();
        let sent =
            tokio::spawn(async move { service.send(&*agent, &user, &session.id, &prompt).await })
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
        for chunk in chunks(&reply) {
            if !say(&chat, &self.log, &chunk).await {
                break;
            }
        }
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
    let sent = chat.say(text).await;
    if let Err(e) = &sent {
        log(&format!("sending a message failed: {e:#}"));
    }
    sent.is_ok()
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
/// signal before polling has started is ignored.
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
    let incoming = Incoming {
        chat_id: message.chat.id.0,
        private: message.chat.is_private(),
        user_id: message.from.as_ref().map(|user| user.id.0),
        text: message.text().map(str::to_string),
    };
    let chat = TelegramChat {
        bot,
        chat: message.chat.id,
    };
    app.handle_isolated(chat, incoming).await;
    Ok(())
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
    let client = agent::client()?;
    let store = Store::open(&store::path())?;
    let service = Arc::new(Service::new(store.clone(), model, log_to_stderr));
    let agent = agent::build(&client, model, service.memory());
    let app = Arc::new(Telegram::new(
        service,
        store,
        agent,
        Arc::new(log_to_stderr),
    ));
    let bot = config.bot();
    let mut dispatcher = dispatcher(bot.clone(), app.clone());
    stop_on(dispatcher.shutdown_token(), stop);
    log_to_stderr("polling for messages; Ctrl-C or SIGTERM stops");
    serve(&mut dispatcher, bot, &app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::agent::{Agent, AgentBuilder};
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use tokio::sync::{Semaphore, mpsc};

    // ---- pure functions ----

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
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
        assert_eq!(chunks("hi"), ["hi"]);
        assert_eq!(chunks(" \n"), ["(The model sent an empty reply.)"]);
        let huge = "x".repeat(MESSAGE_LIMIT * 10);
        let parts = chunks(&huge);
        assert_eq!(parts.len(), MAX_CHUNKS);
        let note = &parts[MAX_CHUNKS - 1];
        assert!(note.starts_with("(Reply cut short: 3 more"), "{note}");
        let exactly = "x".repeat(MESSAGE_LIMIT * MAX_CHUNKS);
        assert_eq!(chunks(&exactly).len(), MAX_CHUNKS);
        assert!(!chunks(&exactly).last().unwrap().starts_with('('));
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
    }

    impl Event {
        fn said(&self) -> Option<&str> {
            match self {
                Event::Say(text) => Some(text),
                Event::Typing => None,
            }
        }
    }

    #[derive(Clone)]
    struct Recorder {
        events: mpsc::UnboundedSender<Event>,
        fail: bool,
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
            if self.fail {
                bail!("blocked by the user");
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
        let store = Store::open_in_memory().unwrap();
        let service = Service::new(store.clone(), "test/model", |_| {});
        let agent = make(&service);
        let logged = Logged::default();
        let sink = logged.clone();
        let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
        // Long enough that no renewal lands mid-test; one test shortens it.
        let app = Telegram::new(Arc::new(service), store.clone(), agent, log)
            .typing_every(Duration::from_secs(3600));
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
        };
        let channel = Incoming {
            chat_id: 5,
            private: true,
            user_id: None,
            text: Some("post".into()),
        };
        let photo = Incoming {
            text: None,
            ..from(9, "")
        };

        for (incoming, expected) in [
            (group, vec![]),
            (channel, vec![]),
            (photo, vec![said(NOT_TEXT)]),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_sends_are_logged_and_stop_the_remaining_chunks() {
        let long = format!("{}\n{}", "a".repeat(4000), "b".repeat(4000));
        let h = harness(vec![MockTurn::text(long)]);
        let (events, mut rx) = mpsc::unbounded_channel();
        let chat = Recorder { events, fail: true };

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
            prompt: &str,
            conversation: &str,
        ) -> Result<rig_agent::agent::PromptResponse, rig_agent::completion::PromptError> {
            self.started.send(()).unwrap();
            self.gate.acquire().await.unwrap().forget();
            self.inner.run(prompt, conversation).await
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
            _: &str,
            _: &str,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_isolated_answers_like_handle() {
        let h = harness(vec![]);
        let (chat, mut rx) = recorder();

        h.app.handle_isolated(chat, from(1, "/usage")).await;

        assert_eq!(rx.try_recv().unwrap(), said("No turns yet."));
    }

    #[test]
    fn logging_to_stderr_is_prefixed() {
        // Output goes to stderr; this only proves it does not panic.
        log_to_stderr("test line");
    }
}
