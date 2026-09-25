//! The core every transport calls. The CLI is one; HTTP and Telegram are
//! meant to be others.
//!
//! A transport turns its own notion of a person into a [`User`] with
//! [`Service::user`], finds or creates a [`Session`], and calls
//! [`Service::send`]. Everything returns values; nothing here prints.
//!
//! Guarantees:
//!
//! - A user only ever sees their own sessions. Asking for someone else's
//!   session id is [`Error::NotFound`], the same as asking for one that does
//!   not exist.
//! - Turns on one session run one at a time, in the order they reach the
//!   session's lock: the second loads the history the first saved. Turns on
//!   different sessions run concurrently.
//! - A turn's transcript is saved before `send` returns its reply. If it
//!   cannot be saved, `send` fails, even though the model answered.
//! - A turn's telemetry is saved after that and its failure only produces a
//!   warning, passed to the callback given to [`Service::new`].
//! - [`Service::send_stream`] and [`Service::send_detached`] keep all of the
//!   above, and run the turn in its own task once it holds the session, so
//!   a caller that goes away mid-turn does not cancel it.

use crate::runner::{self, Run, RunStart, RunStream};
use crate::store::{AppendError, SqliteMemory, Store, now_millis};
use crate::{agent, telemetry};
use futures_util::StreamExt;
use rig_agent::agent::{MultiTurnStreamItem, PromptResponse, StreamingError, StreamingResult};
use rig_agent::completion::{CompletionError, PromptError};
use rig_agent::prelude::Message;
use rig_agent::streaming::StreamedAssistantContent;
use std::sync::Arc;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use tracing::Instrument;

type SessionLock = OwnedMutexGuard<()>;

pub use crate::store::{RunRecord, Session, SessionSummary, SessionUsage, User};

/// Why a service call failed, in terms a transport can map to its own
/// responses: `NotFound` to 404, `AlreadyExists` and `Conflict` to 409,
/// `Invalid` to 400, `Model` to 502, `Storage` to 500.
#[derive(Debug)]
pub enum Error {
    /// No such session for this user.
    NotFound,
    /// The user already has a session with this name.
    AlreadyExists(String),
    /// The request itself is unusable, such as an empty message.
    Invalid(String),
    /// Another process changed the session during this turn. The reply was
    /// not saved; sending the message again will work.
    Conflict(String),
    /// The model or the agent loop failed. Nothing was saved.
    Model(anyhow::Error),
    /// The database failed.
    Storage(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such session"),
            Self::AlreadyExists(name) => write!(f, "a session named `{name}` already exists"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Conflict(why) => write!(f, "{why}; the reply was not saved, send it again"),
            Self::Model(e) => write!(f, "{e:#}"),
            Self::Storage(e) => write!(f, "storage: {e:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// A short, stable name for the kind of failure, for telemetry.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AlreadyExists(_) => "already_exists",
            Self::Invalid(_) => "invalid",
            Self::Conflict(_) => "conflict",
            Self::Model(_) => "model",
            Self::Storage(_) => "storage",
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self::Storage(e)
    }
}

/// A finished turn: the reply, and the run it was recorded as.
#[derive(Debug, Clone)]
pub struct Turn {
    pub reply: String,
    pub run: RunRecord,
}

type Warn = Arc<dyn Fn(&str) + Send + Sync>;

/// Share one per process, behind an `Arc` if several tasks need it.
pub struct Service {
    store: Store,
    model: String,
    warn: Warn,
    /// How many spawned turns are running; see [`Service::idle`].
    in_flight: Arc<watch::Sender<usize>>,
}

fn non_empty(what: &str, value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(Error::Invalid(format!("{what} must not be empty")));
    }
    Ok(())
}

impl Service {
    /// `model` is recorded on every run. `warn` receives problems that do
    /// not fail the call, such as a telemetry row that could not be saved.
    pub fn new(
        store: Store,
        model: impl Into<String>,
        warn: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            model: model.into(),
            warn: Arc::new(warn),
            in_flight: Arc::new(watch::Sender::new(0)),
        }
    }

    /// The memory to build this service's agent with:
    /// `AgentBuilder::memory(service.memory())`.
    pub fn memory(&self) -> SqliteMemory {
        self.store.memory()
    }

    /// The user a transport knows by `external_id`, created on first sight.
    pub async fn user(&self, transport: &str, external_id: &str) -> Result<User, Error> {
        non_empty("transport", transport)?;
        non_empty("user id", external_id)?;
        let (transport, external_id) = (transport.to_string(), external_id.to_string());
        Ok(self
            .store
            .call(move |s| s.user(&transport, &external_id))
            .await?)
    }

    /// A new, empty session. Fails if the user already has one by this name.
    pub async fn create_session(&self, user: &User, name: &str) -> Result<Session, Error> {
        non_empty("session name", name)?;
        let (owner, wanted) = (user.clone(), name.to_string());
        self.store
            .call(move |s| s.create_session(&owner, &wanted))
            .await?
            .ok_or_else(|| Error::AlreadyExists(name.to_string()))
    }

    /// The user's session called `name`, created if they have none.
    pub async fn open_session(&self, user: &User, name: &str) -> Result<Session, Error> {
        non_empty("session name", name)?;
        let (owner, wanted) = (user.clone(), name.to_string());
        Ok(self
            .store
            .call(move |s| s.open_session(&owner, &wanted))
            .await?)
    }

    /// The user's session with this id.
    pub async fn session(&self, user: &User, session_id: &str) -> Result<Session, Error> {
        let (owner, id) = (user.clone(), session_id.to_string());
        self.store
            .call(move |s| s.session(&owner, &id))
            .await?
            .ok_or(Error::NotFound)
    }

    /// The user's sessions, by name, with their message counts.
    pub async fn sessions(&self, user: &User) -> Result<Vec<SessionSummary>, Error> {
        let owner = user.clone();
        Ok(self.store.call(move |s| s.sessions(&owner)).await?)
    }

    /// Token totals for each of the user's sessions that has run.
    pub async fn usage(&self, user: &User) -> Result<Vec<SessionUsage>, Error> {
        let owner = user.clone();
        Ok(self.store.call(move |s| s.usage(&owner)).await?)
    }

    /// The transcript of one of the user's sessions, oldest first.
    pub async fn history(&self, user: &User, session_id: &str) -> Result<Vec<Message>, Error> {
        let session = self.session(user, session_id).await?;
        Ok(self.store.call(move |s| s.load(&session.id)).await?)
    }

    /// Run one turn: `text` from `user` in their session `session_id`.
    ///
    /// `agent` must have been built with [`Service::memory`]: Rig loads the
    /// history and saves the turn through it. An agent without it is an
    /// error, not a turn that silently forgets.
    ///
    /// Waits while another turn on the same session is running. Dropping the
    /// returned future cancels the turn; a transport that must not lose a
    /// paid-for reply when its client goes away should use
    /// [`Service::send_detached`] or [`Service::send_stream`].
    pub async fn send<R: Run>(
        &self,
        agent: &R,
        user: &User,
        session_id: &str,
        text: &str,
    ) -> Result<Turn, Error> {
        let (session, lock) = self.claim(user, session_id, text).await?;
        let span = turn_span(user, &session);
        self.complete(&session, lock, agent.run(text, &session.id))
            .instrument(span)
            .await
    }

    /// [`Service::send`], run in its own task so the turn finishes and is
    /// saved even if the caller stops waiting for it.
    ///
    /// Checking the request and waiting for the session happen in the
    /// caller's future, so a caller that gives up while queued behind another
    /// turn leaves nothing behind. Once the session is theirs, the turn runs
    /// to the end, and [`Service::idle`] waits for it.
    pub async fn send_detached<R: Run + 'static>(
        self: &Arc<Self>,
        agent: Arc<R>,
        user: &User,
        session_id: &str,
        text: &str,
    ) -> Result<Turn, Error> {
        let (session, lock) = self.claim(user, session_id, text).await?;
        let span = turn_span(user, &session);
        let (service, text) = (self.clone(), text.to_string());
        self.spawn(
            async move {
                let run = agent.run(&text, &session.id);
                service.complete(&session, lock, run).await
            }
            .instrument(span),
        )
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }

    /// Run one turn and report it as it happens: text deltas and tool calls,
    /// then [`TurnEvent::Done`] with what [`Service::send`] would have
    /// returned.
    ///
    /// Everything `send` guarantees holds: the same session lock, the
    /// transcript saved and checked before `Done` reports success, the run
    /// recorded whether or not it succeeded.
    ///
    /// Returns once the request is checked and the session is this turn's,
    /// so an empty message or someone else's session is an error here, not
    /// an event. The turn then runs in its own task: the [`TurnStream`] is
    /// only a view of it, and dropping it (a client that disconnects) does
    /// not stop the turn from finishing and being saved.
    pub async fn send_stream<R: RunStream + 'static>(
        self: &Arc<Self>,
        agent: Arc<R>,
        user: &User,
        session_id: &str,
        text: &str,
    ) -> Result<TurnStream, Error> {
        let (session, lock) = self.claim(user, session_id, text).await?;
        let span = turn_span(user, &session);
        let (events, receiver) = mpsc::unbounded_channel();
        let (service, text) = (self.clone(), text.to_string());
        // The handle is not needed: the task reports through `events`, and a
        // panic in it closes the channel without a `Done`.
        drop(
            self.spawn(
                async move {
                    let run = relay(agent.stream(&text, &session.id), &events);
                    let outcome = service.complete(&session, lock, run).await;
                    // The receiver may be gone; the turn is saved either way.
                    let _ = events.send(TurnEvent::Done(outcome));
                }
                .instrument(span),
            ),
        );
        Ok(TurnStream { events: receiver })
    }

    /// Resolves once no turn started by [`Service::send_detached`] or
    /// [`Service::send_stream`] is still running. A server calls this after
    /// it stops accepting requests, so shutting down never cuts a turn off.
    pub async fn idle(&self) {
        let mut running = self.in_flight.subscribe();
        // Cannot fail: the service holds the sender it is subscribed to.
        let _ = running.wait_for(|n| *n == 0).await;
    }

    /// Spawn `task`, counted as in flight until it ends, even by a panic.
    fn spawn<T: Send + 'static>(
        &self,
        task: impl Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        // Counted before the spawn, so `idle` cannot miss a task that has
        // not been polled yet.
        let counted = InFlight::enter(&self.in_flight);
        tokio::spawn(async move {
            let _counted = counted;
            task.await
        })
    }

    /// Check a turn's request and wait for its session. Nothing has started
    /// yet, so dropping this future is harmless.
    async fn claim(
        &self,
        user: &User,
        session_id: &str,
        text: &str,
    ) -> Result<(Session, SessionLock), Error> {
        non_empty("message", text)?;
        let session = self.session(user, session_id).await?;
        let lock = self.store.lock_session(&session.id).await;
        Ok((session, lock))
    }

    /// Run a claimed turn and record it. `run` must not touch the memory
    /// before it is first polled; see [`RunStream`].
    async fn complete(
        &self,
        session: &Session,
        _lock: SessionLock,
        run: impl Future<Output = Result<PromptResponse, PromptError>>,
    ) -> Result<Turn, Error> {
        self.store.begin_turn(&session.id);
        let started_at = now_millis();

        let outcome = run.await;

        let receipt = self.store.take_receipt(&session.id);
        let first_seq = match receipt.loaded_next {
            Some(next) => next,
            None => {
                let id = session.id.clone();
                self.store.call(move |s| s.next_seq(&id)).await?
            }
        };
        let start = RunStart {
            run_id: uuid::Uuid::new_v4().to_string(),
            session_id: &session.id,
            model: &self.model,
            started_at,
            first_seq,
        };
        let (reply, run) = settle(start, outcome, receipt.appended);
        record_on_span(&tracing::Span::current(), &reply, &run);

        let row = run.clone();
        if let Err(e) = self.store.call(move |s| s.save_run(&row)).await {
            (self.warn)(&format!("run telemetry not saved: {e}"));
        }
        reply.map(|reply| Turn { reply, run })
    }
}

/// The span one turn runs in: Athena's `invoke_agent`, which Rig adopts
/// instead of opening its own, so its `chat` and `execute_tool` spans nest
/// under it. Created in the caller's context, so an HTTP request's span is
/// its parent even when the turn then runs in its own task.
fn turn_span(user: &User, session: &Session) -> tracing::Span {
    tracing::info_span!(
        "invoke_agent",
        otel.name = format!("invoke_agent {}", agent::NAME),
        otel.status_code = tracing::field::Empty,
        gen_ai.operation.name = "invoke_agent",
        gen_ai.agent.name = agent::NAME,
        gen_ai.conversation.id = session.id.as_str(),
        // Rig records the prompt here when content telemetry is on.
        gen_ai.prompt = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        error.type = tracing::field::Empty,
        athena.run_id = tracing::field::Empty,
        athena.transport = user.transport(),
        enduser.pseudo.id = telemetry::pseudonym(user),
    )
}

/// Put what the turn came to on its span. Rig records usage only on spans
/// it opened itself, so the run's totals are copied here.
fn record_on_span(span: &tracing::Span, reply: &Result<String, Error>, run: &RunRecord) {
    span.record("athena.run_id", run.run_id.as_str());
    span.record("gen_ai.usage.input_tokens", run.input_tokens);
    span.record("gen_ai.usage.output_tokens", run.output_tokens);
    if let Err(e) = reply {
        span.record("otel.status_code", "ERROR");
        span.record("error.type", e.kind());
    }
}

/// Something that happened during a streamed turn.
// `Done` is sent once per turn; boxing it would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TurnEvent {
    /// Assistant text, as it arrives. Text the model writes alongside a tool
    /// call is included, so the deltas are not always the reply: `Done`'s
    /// reply is.
    Text(String),
    /// The model asked for a tool.
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// The turn is over; always the last event.
    Done(Result<Turn, Error>),
}

/// The events of one streamed turn. See [`Service::send_stream`].
pub struct TurnStream {
    events: mpsc::UnboundedReceiver<TurnEvent>,
}

impl TurnStream {
    /// The next event. `None` after `Done`, or if the turn's task panicked
    /// before it could send one.
    pub async fn next(&mut self) -> Option<TurnEvent> {
        self.events.recv().await
    }
}

/// Drive Rig's stream to its end, forwarding what the client should see,
/// and return what the blocking surface would have returned.
async fn relay(
    stream: impl Future<Output = StreamingResult>,
    events: &mpsc::UnboundedSender<TurnEvent>,
) -> Result<PromptResponse, PromptError> {
    let mut stream = stream.await;
    while let Some(item) = stream.next().await {
        // A failed send means the client is gone; the turn carries on.
        let event = match item {
            Ok(MultiTurnStreamItem::FinalResponse(response)) => return Ok(response),
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                TurnEvent::Text(text.text)
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => TurnEvent::ToolCall {
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
            },
            // Reasoning, tool-call deltas, tool results and per-call usage
            // are not surfaced. `ModelTurnRetried` only happens when a hook
            // rejects a turn, and this agent registers none.
            Ok(_) => continue,
            // Rig's own conversion back to the blocking surface's error.
            Err(StreamingError::Completion(e)) => return Err(PromptError::CompletionError(e)),
            Err(StreamingError::Prompt(e)) => return Err(*e),
        };
        let _ = events.send(event);
    }
    Err(PromptError::CompletionError(
        CompletionError::ResponseError("the model's stream ended without a final response".into()),
    ))
}

/// Counts spawned turns; see [`Service::idle`].
struct InFlight(Arc<watch::Sender<usize>>);

impl InFlight {
    fn enter(count: &Arc<watch::Sender<usize>>) -> Self {
        count.send_modify(|n| *n += 1);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

/// Decide what a finished run means: the reply or an error, and its record.
///
/// A reply only counts once its transcript is saved. Rig logs a failed
/// memory append and returns the reply anyway, so the append's own result,
/// from the memory's receipt, is what decides.
fn settle(
    start: RunStart<'_>,
    outcome: Result<PromptResponse, PromptError>,
    appended: Option<Result<(i64, i64), AppendError>>,
) -> (Result<String, Error>, RunRecord) {
    let response = match outcome {
        Ok(response) => response,
        Err(e) => {
            let run = runner::record(start, None, Err(e.to_string()));
            let error = match e {
                // The memory is the database: a failed load is ours, not the model's.
                PromptError::MemoryError(_) => Error::Storage(e.into()),
                _ => Error::Model(e.into()),
            };
            return (Err(error), run);
        }
    };
    match appended {
        Some(Ok((_, last_seq))) => {
            let run = runner::record(start, Some(&response), Ok(last_seq));
            (Ok(response.output), run)
        }
        Some(Err(e)) => {
            let run = runner::record(start, Some(&response), Err(e.to_string()));
            let error = match e {
                AppendError::Conflict { .. } => Error::Conflict(e.to_string()),
                AppendError::Storage(_) => Error::Storage(e.into()),
            };
            (Err(error), run)
        }
        None => {
            let why = "the agent has no conversation memory from this store, so the \
                       turn was not saved; build it with AgentBuilder::memory(service.memory())";
            let run = runner::record(start, Some(&response), Err(why.into()));
            (Err(Error::Storage(anyhow::anyhow!(why))), run)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent;
    use futures_util::FutureExt;
    use rig_agent::agent::{Agent, AgentBuilder};
    use rig_core::memory::{ConversationMemory, MemoryError};
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
    use rig_core::wasm_compat::WasmBoxedFuture;
    use std::sync::Mutex;
    use tokio::sync::{Barrier, Semaphore, mpsc};

    type Warnings = Arc<Mutex<Vec<String>>>;

    fn service() -> (Service, Store, Warnings) {
        let store = Store::open_in_memory().unwrap();
        let warnings = Warnings::default();
        let sink = warnings.clone();
        let service = Service::new(store.clone(), "test/model", move |w| {
            sink.lock().unwrap().push(w.to_string())
        });
        (service, store, warnings)
    }

    fn agent_with(
        memory: impl ConversationMemory + 'static,
        turns: impl IntoIterator<Item = MockTurn>,
    ) -> (Agent, MockCompletionModel) {
        let model = MockCompletionModel::new(turns);
        (
            agent::configure(AgentBuilder::new(model.clone()).memory(memory)),
            model,
        )
    }

    fn add_turns() -> Vec<MockTurn> {
        vec![
            MockTurn::tool_call("call_1", "add", serde_json::json!({"a": 21, "b": 21})),
            MockTurn::text("42"),
        ]
    }

    async fn with_session(service: &Service) -> (User, Session) {
        let user = service.user("cli", "local").await.unwrap();
        let session = service.open_session(&user, "s").await.unwrap();
        (user, session)
    }

    type RunRow = (i64, i64, String, Option<String>, i64);

    /// first_seq, last_seq, status, error, input_tokens of every run, oldest first.
    async fn run_rows(store: &Store) -> Vec<RunRow> {
        store
            .call(|s| {
                let db = s.db_for_tests();
                let mut q = db
                    .prepare(
                        "SELECT first_seq, last_seq, status, error, input_tokens
                         FROM runs ORDER BY started_at, rowid",
                    )
                    .unwrap();
                q.query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            })
            .await
    }

    #[test]
    fn every_error_has_a_distinct_kind_for_telemetry() {
        let kinds = [
            Error::NotFound.kind(),
            Error::AlreadyExists("n".into()).kind(),
            Error::Invalid("i".into()).kind(),
            Error::Conflict("c".into()).kind(),
            Error::Model(anyhow::anyhow!("m")).kind(),
            Error::Storage(anyhow::anyhow!("s")).kind(),
        ];
        assert_eq!(
            kinds,
            [
                "not_found",
                "already_exists",
                "invalid",
                "conflict",
                "model",
                "storage"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_is_saved_before_its_reply_is_returned() {
        let (service, store, warnings) = service();
        let (user, session) = with_session(&service).await;
        let (agent, model) = agent_with(service.memory(), add_turns());

        let turn = service
            .send(&agent, &user, &session.id, "add 21 and 21")
            .await
            .unwrap();

        assert_eq!(turn.reply, "42");
        assert_eq!(
            (turn.run.first_seq, turn.run.last_seq, turn.run.model_calls),
            (0, 3, 2)
        );
        assert_eq!(turn.run.status, "ok");
        assert_eq!(turn.run.model, "test/model");
        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 4);
        assert_eq!(model.request_count(), 2);
        assert_eq!(run_rows(&store).await, [(0, 3, "ok".into(), None, 0)]);
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_users_session_is_not_found_and_never_reaches_the_model() {
        let (service, store, _) = service();
        let (owner, session) = with_session(&service).await;
        let stranger = service.user("telegram", "99").await.unwrap();
        let (agent, model) = agent_with(service.memory(), [MockTurn::text("leak")]);

        let send = service.send(&agent, &stranger, &session.id, "hi").await;
        let read = service.history(&stranger, &session.id).await;

        assert!(matches!(send, Err(Error::NotFound)), "{send:?}");
        assert!(matches!(read, Err(Error::NotFound)), "{read:?}");
        assert_eq!(model.request_count(), 0);
        assert!(run_rows(&store).await.is_empty());
        assert!(service.sessions(&stranger).await.unwrap().is_empty());
        assert_eq!(service.sessions(&owner).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_input_is_refused_before_anything_is_written() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let (agent, model) = agent_with(service.memory(), []);

        for (result, what) in [
            (service.user("", "x").await.map(|_| ()), "transport"),
            (service.user("cli", " ").await.map(|_| ()), "user id"),
            (
                service.create_session(&user, "").await.map(|_| ()),
                "session name",
            ),
            (
                service.open_session(&user, "  ").await.map(|_| ()),
                "session name",
            ),
            (
                service
                    .send(&agent, &user, &session.id, " \n")
                    .await
                    .map(|_| ()),
                "message",
            ),
        ] {
            let err = result.unwrap_err();
            assert!(matches!(err, Error::Invalid(_)), "{err:?}");
            assert_eq!(err.to_string(), format!("{what} must not be empty"));
        }
        assert_eq!(model.request_count(), 0);
        assert!(run_rows(&store).await.is_empty());
        assert_eq!(service.sessions(&user).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_name_can_be_created_once_per_user() {
        let (service, _, _) = service();
        let user = service.user("http", "a").await.unwrap();

        let created = service.create_session(&user, "notes").await.unwrap();
        let again = service.create_session(&user, "notes").await.unwrap_err();

        assert_eq!(again.to_string(), "a session named `notes` already exists");
        assert_eq!(service.session(&user, &created.id).await.unwrap(), created);
        assert_eq!(service.open_session(&user, "notes").await.unwrap(), created);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_error_saves_nothing_and_is_recorded() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let (agent, _) = agent_with(service.memory(), [MockTurn::error("upstream unavailable")]);

        let err = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Model(_)), "{err:?}");
        assert!(err.to_string().contains("upstream unavailable"), "{err}");
        assert!(
            service
                .history(&user, &session.id)
                .await
                .unwrap()
                .is_empty()
        );
        let runs = run_rows(&store).await;
        assert_eq!((runs[0].0, runs[0].1, runs[0].2.as_str()), (0, -1, "error"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_history_that_cannot_be_read_is_a_storage_error_not_a_model_error() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let id = session.id.clone();
        store
            .call(move |s| {
                s.db_for_tests()
                    .execute("INSERT INTO messages VALUES (?1, 0, 'not json')", [&id])
                    .unwrap()
            })
            .await;
        let (agent, model) = agent_with(service.memory(), [MockTurn::text("unreached")]);

        let err = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        assert!(err.to_string().starts_with("storage: "), "{err}");
        assert_eq!(model.request_count(), 0);
        // Recorded where the next message would have gone.
        assert_eq!(run_rows(&store).await[0].0, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_whose_transcript_cannot_be_saved_is_an_error() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        store
            .call(|s| {
                s.db_for_tests()
                    .execute_batch(
                        "CREATE TRIGGER fail BEFORE INSERT ON messages
                         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                    )
                    .unwrap()
            })
            .await;
        let (agent, _) = agent_with(
            service.memory(),
            [
                MockTurn::text("lost").with_usage(rig_core::completion::Usage {
                    input_tokens: 7,
                    ..Default::default()
                }),
            ],
        );

        let err = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap_err();

        // Rig itself returned "lost" as a success; the receipt caught it.
        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        assert!(err.to_string().contains("disk full"), "{err}");
        // The tokens were spent, so the run records them.
        assert_eq!(
            run_rows(&store).await,
            [(
                0,
                -1,
                "error".into(),
                Some("transcript not saved: disk full".into()),
                7
            )]
        );
    }

    /// Stands in for another process: appends to the session right after
    /// this turn's load, before its append.
    struct InterleavedWriter {
        inner: SqliteMemory,
        store: Store,
    }

    impl ConversationMemory for InterleavedWriter {
        fn load<'a>(
            &'a self,
            id: &'a str,
        ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
            Box::pin(async move {
                let history = self.inner.load(id).await;
                let other = id.to_string();
                self.store
                    .call(move |s| s.append(&other, None, &[Message::user("theirs")]))
                    .await
                    .unwrap();
                history
            })
        }

        fn append<'a>(
            &'a self,
            id: &'a str,
            messages: Vec<Message>,
        ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.append(id, messages)
        }

        fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.clear(id)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_process_writing_mid_turn_is_a_conflict_not_an_interleaving() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let memory = InterleavedWriter {
            inner: service.memory(),
            store: store.clone(),
        };
        assert!(memory.clear(&session.id).await.is_err());
        let (agent, _) = agent_with(memory, [MockTurn::text("mine")]);

        let err = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        assert!(err.to_string().ends_with("send it again"), "{err}");
        assert_eq!(
            service.history(&user, &session.id).await.unwrap(),
            [Message::user("theirs")]
        );
        assert_eq!(run_rows(&store).await[0].2, "error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_without_this_stores_memory_is_refused_after_the_fact() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let model = MockCompletionModel::new([MockTurn::text("forgotten")]);
        let agent = agent::configure(AgentBuilder::new(model));

        let err = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("no conversation memory"), "{err}");
        assert!(
            service
                .history(&user, &session.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(run_rows(&store).await[0].2, "error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_telemetry_row_is_a_warning_and_the_reply_still_arrives() {
        let (service, store, warnings) = service();
        let (user, session) = with_session(&service).await;
        store
            .call(|s| s.db_for_tests().execute_batch("DROP TABLE runs").unwrap())
            .await;
        let (agent, _) = agent_with(service.memory(), [MockTurn::text("hello")]);

        let turn = service
            .send(&agent, &user, &session.id, "hi")
            .await
            .unwrap();

        assert_eq!(turn.reply, "hello");
        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 2);
        assert!(service.usage(&user).await.is_err());
        let warnings = warnings.lock().unwrap().clone();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with("run telemetry not saved"),
            "{warnings:?}"
        );
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        for (error, text) in [
            (Error::NotFound, "no such session"),
            (Error::Invalid("bad".into()), "bad"),
            (
                Error::Model(anyhow::anyhow!("outer").context("ctx")),
                "ctx: outer",
            ),
            (Error::from(anyhow::anyhow!("gone")), "storage: gone"),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    /// Parks each load after it has read the history, until released, and
    /// reports how many messages it saw.
    struct Gated {
        inner: SqliteMemory,
        gate: Arc<Semaphore>,
        loaded: mpsc::UnboundedSender<usize>,
    }

    impl ConversationMemory for Gated {
        fn load<'a>(
            &'a self,
            id: &'a str,
        ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
            Box::pin(async move {
                let history = self.inner.load(id).await?;
                self.loaded.send(history.len()).unwrap();
                self.gate.acquire().await.unwrap().forget();
                Ok(history)
            })
        }

        fn append<'a>(
            &'a self,
            id: &'a str,
            messages: Vec<Message>,
        ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.append(id, messages)
        }

        fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.clear(id)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_turn_on_a_session_waits_for_the_first_and_sees_its_result() {
        let (service, store, _) = service();
        let (user, session) = with_session(&service).await;
        let gate = Arc::new(Semaphore::new(0));
        let (loaded, mut loads) = mpsc::unbounded_channel();
        let memory = Gated {
            inner: service.memory(),
            gate: gate.clone(),
            loaded,
        };
        assert!(memory.clear(&session.id).await.is_err());
        let (agent, _) = agent_with(memory, [MockTurn::text("one"), MockTurn::text("two")]);

        let control = async {
            // One turn has loaded the empty session and is parked mid-turn.
            assert_eq!(loads.recv().await, Some(0));
            // Wait until the other is queued on the session's lock. If it
            // loads instead, the lock is not doing its job.
            // Re-polled until true. Written without branches so which poll
            // finds it true does not change which lines run.
            let queued = std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                let waiting = store.turns_on(&session.id) == 2;
                waiting
                    .then_some(())
                    .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
            });
            let outcome = tokio::select! {
                second = loads.recv() => Err(second),
                () = queued => Ok(()),
            };
            assert_eq!(outcome, Ok(()), "a turn loaded mid-turn: {outcome:?}");
            gate.add_permits(1);
            // It loads only once the first turn is saved, and sees all of it.
            assert_eq!(loads.recv().await, Some(2));
            gate.add_permits(1);
        };
        let (a, b, ()) = tokio::join!(
            service.send(&agent, &user, &session.id, "first"),
            service.send(&agent, &user, &session.id, "second"),
            control,
        );

        let mut replies = [a.unwrap().reply, b.unwrap().reply];
        replies.sort();
        assert_eq!(replies, ["one", "two"]);
        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 4);
        let ranges: Vec<(i64, i64)> = run_rows(&store).await.iter().map(|r| (r.0, r.1)).collect();
        assert_eq!(ranges, [(0, 1), (2, 3)]);
    }

    /// Each load waits until every other load has started too. If turns on
    /// different sessions were serialized, this would never finish.
    struct Together {
        inner: SqliteMemory,
        barrier: Arc<Barrier>,
    }

    impl ConversationMemory for Together {
        fn load<'a>(
            &'a self,
            id: &'a str,
        ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
            Box::pin(async move {
                self.barrier.wait().await;
                self.inner.load(id).await
            })
        }

        fn append<'a>(
            &'a self,
            id: &'a str,
            messages: Vec<Message>,
        ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.append(id, messages)
        }

        fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.inner.clear(id)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turns_on_different_sessions_run_at_the_same_time() {
        let (service, _, _) = service();
        let user = service.user("telegram", "1").await.unwrap();
        let a = service.open_session(&user, "a").await.unwrap();
        let b = service.open_session(&user, "b").await.unwrap();
        let memory = Together {
            inner: service.memory(),
            barrier: Arc::new(Barrier::new(2)),
        };
        assert!(memory.clear(&a.id).await.is_err());
        let (agent, _) = agent_with(memory, [MockTurn::text("x"), MockTurn::text("y")]);

        // The timeout only turns a deadlock into a failure; passing does not
        // depend on timing.
        let both = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(
                service.send(&agent, &user, &a.id, "to a"),
                service.send(&agent, &user, &b.id, "to b"),
            )
        })
        .await
        .expect("turns on different sessions were serialized");

        assert!(both.0.is_ok() && both.1.is_ok());
        assert_eq!(service.history(&user, &a.id).await.unwrap().len(), 2);
        assert_eq!(service.history(&user, &b.id).await.unwrap().len(), 2);
    }

    // ---- streamed and detached turns ----

    fn stream_agent_with(
        memory: impl ConversationMemory + 'static,
        turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
    ) -> (Arc<Agent>, MockCompletionModel) {
        let model = MockCompletionModel::from_stream_turns(turns);
        (
            Arc::new(agent::configure(
                AgentBuilder::new(model.clone()).memory(memory),
            )),
            model,
        )
    }

    fn usage(input_tokens: u64) -> rig_core::completion::Usage {
        rig_core::completion::Usage {
            input_tokens,
            ..Default::default()
        }
    }

    /// `add 21 and 21`, streamed: a tool call, then the answer in two deltas.
    fn streamed_add() -> Vec<Vec<MockStreamEvent>> {
        vec![
            vec![
                MockStreamEvent::tool_call("call_1", "add", serde_json::json!({"a": 21, "b": 21})),
                MockStreamEvent::final_response(usage(111)),
            ],
            vec![
                MockStreamEvent::text("4"),
                MockStreamEvent::text("2"),
                MockStreamEvent::final_response(usage(145)),
            ],
        ]
    }

    /// Every event of a streamed turn, in order.
    async fn drain(mut turn: TurnStream) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        while let Some(event) = turn.next().await {
            events.push(event);
        }
        events
    }

    /// The outcome carried by `Done`, which must be the last event.
    fn done(events: &[TurnEvent]) -> &Result<Turn, Error> {
        let last = matches!(events.last(), Some(TurnEvent::Done(_)));
        assert!(last, "the last event is not Done: {events:?}");
        let outcome = events.iter().find_map(|e| match e {
            TurnEvent::Done(outcome) => Some(outcome),
            _ => None,
        });
        outcome.expect("checked above")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_turn_reports_as_it_goes_and_is_saved_like_send() {
        let (service, store, warnings) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let (agent, model) = stream_agent_with(service.memory(), streamed_add());

        let turn = service
            .send_stream(agent, &user, &session.id, "add 21 and 21")
            .await
            .unwrap();
        let events = drain(turn).await;

        assert_eq!(events.len(), 4, "{events:?}");
        let args = serde_json::json!({"a": 21, "b": 21});
        let tool_call = matches!(&events[0], TurnEvent::ToolCall { name, arguments }
                if name == "add" && *arguments == args);
        assert!(tool_call, "{events:?}");
        assert!(matches!(&events[1], TurnEvent::Text(t) if t == "4"));
        assert!(matches!(&events[2], TurnEvent::Text(t) if t == "2"));
        let turn = done(&events).as_ref().unwrap();
        assert_eq!(turn.reply, "42");
        assert_eq!(
            (turn.run.first_seq, turn.run.last_seq, turn.run.model_calls),
            (0, 3, 2)
        );
        assert_eq!(model.request_count(), 2);
        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 4);
        assert_eq!(run_rows(&store).await, [(0, 3, "ok".into(), None, 256)]);
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_request_is_checked_before_the_turn_starts() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (owner, session) = with_session(&service).await;
        let stranger = service.user("http", "stranger").await.unwrap();
        let (agent, model) = stream_agent_with(service.memory(), streamed_add());

        let theirs = service
            .send_stream(agent.clone(), &stranger, &session.id, "hi")
            .await;
        let empty = service.send_stream(agent, &owner, &session.id, " ").await;

        assert!(matches!(theirs, Err(Error::NotFound)));
        assert!(matches!(empty, Err(Error::Invalid(_))));
        service.idle().await;
        assert_eq!(model.request_count(), 0);
        assert!(run_rows(&store).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_provider_error_is_a_model_error_and_is_recorded() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let (agent, _) = stream_agent_with(
            service.memory(),
            [vec![
                MockStreamEvent::text("par"),
                MockStreamEvent::error("upstream unavailable"),
            ]],
        );

        let turn = service
            .send_stream(agent, &user, &session.id, "hi")
            .await
            .unwrap();
        let events = drain(turn).await;

        assert!(matches!(&events[0], TurnEvent::Text(t) if t == "par"));
        let err = done(&events).as_ref().unwrap_err();
        assert!(matches!(err, Error::Model(_)), "{err:?}");
        assert!(err.to_string().contains("upstream unavailable"), "{err}");
        assert!(
            service
                .history(&user, &session.id)
                .await
                .unwrap()
                .is_empty()
        );
        let runs = run_rows(&store).await;
        assert_eq!((runs[0].0, runs[0].1, runs[0].2.as_str()), (0, -1, "error"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_history_that_cannot_be_read_is_a_storage_error() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let id = session.id.clone();
        store
            .call(move |s| {
                s.db_for_tests()
                    .execute("INSERT INTO messages VALUES (?1, 0, 'not json')", [&id])
                    .unwrap()
            })
            .await;
        let (agent, model) = stream_agent_with(service.memory(), streamed_add());

        let turn = service
            .send_stream(agent, &user, &session.id, "hi")
            .await
            .unwrap();
        let events = drain(turn).await;

        assert_eq!(events.len(), 1, "{events:?}");
        let err = done(&events).as_ref().unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        assert_eq!(model.request_count(), 0);
        assert_eq!(run_rows(&store).await[0].2, "error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_reply_whose_transcript_cannot_be_saved_is_an_error() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        store
            .call(|s| {
                s.db_for_tests()
                    .execute_batch(
                        "CREATE TRIGGER fail BEFORE INSERT ON messages
                         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                    )
                    .unwrap()
            })
            .await;
        let (agent, _) = stream_agent_with(
            service.memory(),
            [vec![
                MockStreamEvent::text("lost"),
                MockStreamEvent::final_response(usage(7)),
            ]],
        );

        let turn = service
            .send_stream(agent, &user, &session.id, "hi")
            .await
            .unwrap();
        let events = drain(turn).await;

        // The client saw the text, but the turn is not reported as a success.
        assert!(matches!(&events[0], TurnEvent::Text(t) if t == "lost"));
        let err = done(&events).as_ref().unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(
            run_rows(&store).await,
            [(
                0,
                -1,
                "error".into(),
                Some("transcript not saved: disk full".into()),
                7
            )]
        );
    }

    /// A streaming agent whose stream ends without Rig's final item.
    struct Truncated;

    impl RunStream for Truncated {
        async fn stream(&self, _: &str, _: &str) -> StreamingResult {
            Box::pin(futures_util::stream::empty())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_ends_without_a_final_response_is_a_model_error() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;

        let turn = service
            .send_stream(Arc::new(Truncated), &user, &session.id, "hi")
            .await
            .unwrap();
        let events = drain(turn).await;

        let err = done(&events).as_ref().unwrap_err();
        assert!(matches!(err, Error::Model(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("without a final response"), "{text}");
        assert_eq!(run_rows(&store).await[0].2, "error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_turn_waits_for_a_running_turn_and_sees_its_result() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let gate = Arc::new(Semaphore::new(0));
        let (loaded, mut loads) = mpsc::unbounded_channel();
        let memory = Gated {
            inner: service.memory(),
            gate: gate.clone(),
            loaded,
        };
        let (blocking, _) = agent_with(memory, [MockTurn::text("one")]);
        let (streaming, model) = stream_agent_with(
            service.memory(),
            [vec![
                MockStreamEvent::text("two"),
                MockStreamEvent::final_response(usage(1)),
            ]],
        );
        let watched = model.clone();

        let second = async {
            // Only once the blocking turn holds the session, has loaded it
            // empty and is parked, does the streamed turn begin.
            assert_eq!(loads.recv().await, Some(0));
            let streamed = async {
                let turn = service
                    .send_stream(streaming, &user, &session.id, "second")
                    .await
                    .unwrap();
                drain(turn).await
            };
            // Wait until the streamed turn is queued on the session's lock,
            // or has reached the model, which it must not while the first
            // turn holds the session. Branch-free, like the test above.
            let release = async {
                let queued = std::future::poll_fn(|cx| {
                    cx.waker().wake_by_ref();
                    let waiting = store.turns_on(&session.id) == 2;
                    let ran = watched.request_count() > 0;
                    (waiting || ran)
                        .then_some(waiting)
                        .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
                });
                let waited = queued.await;
                assert!(waited, "a streamed turn ran while the session was held");
                gate.add_permits(1);
            };
            tokio::join!(streamed, release).0
        };
        let (first, events) =
            tokio::join!(service.send(&blocking, &user, &session.id, "first"), second,);

        assert_eq!(first.unwrap().reply, "one");
        assert_eq!(done(&events).as_ref().unwrap().reply, "two");
        // Preamble, the first turn's two messages, then its own prompt.
        assert_eq!(model.requests()[0].chat_history.len(), 4);
        let ranges: Vec<(i64, i64)> = run_rows(&store).await.iter().map(|r| (r.0, r.1)).collect();
        assert_eq!(ranges, [(0, 1), (2, 3)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streamed_turn_nobody_watches_still_finishes_and_idle_waits_for_it() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let gate = Arc::new(Semaphore::new(0));
        let (loaded, mut loads) = mpsc::unbounded_channel();
        let memory = Gated {
            inner: service.memory(),
            gate: gate.clone(),
            loaded,
        };
        let (agent, _) = stream_agent_with(
            memory,
            [vec![
                MockStreamEvent::text("hello"),
                MockStreamEvent::final_response(usage(3)),
            ]],
        );

        let turn = service
            .send_stream(agent, &user, &session.id, "hi")
            .await
            .unwrap();
        assert_eq!(loads.recv().await, Some(0));
        // The client goes away while the turn is parked mid-way.
        drop(turn);

        // One poll is enough: the turn is counted until it ends.
        assert!(service.idle().now_or_never().is_none());
        gate.add_permits(1);
        service.idle().await;

        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 2);
        assert_eq!(run_rows(&store).await, [(0, 1, "ok".into(), None, 3)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_detached_turn_finishes_after_its_caller_gives_up() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let gate = Arc::new(Semaphore::new(0));
        let (loaded, mut loads) = mpsc::unbounded_channel();
        let memory = Gated {
            inner: service.memory(),
            gate: gate.clone(),
            loaded,
        };
        let (agent, _) = agent_with(memory, add_turns());
        let agent = Arc::new(agent);

        // The caller waits until the turn has started, then stops waiting.
        let caller = service.send_detached(agent.clone(), &user, &session.id, "add");
        let outcome = tokio::select! {
            turn = caller => Err(turn),
            loaded = loads.recv() => Ok(loaded),
        };
        // The turn loaded the empty session and is parked; it cannot be done.
        assert!(matches!(outcome, Ok(Some(0))), "{outcome:?}");
        assert!(service.idle().now_or_never().is_none());
        gate.add_permits(1);
        service.idle().await;

        assert_eq!(service.history(&user, &session.id).await.unwrap().len(), 4);
        assert_eq!(run_rows(&store).await[0].2, "ok");

        // And when the caller does wait, it gets what `send` returns.
        let (agent, _) = agent_with(service.memory(), [MockTurn::text("again")]);
        let turn = service
            .send_detached(Arc::new(agent), &user, &session.id, "more")
            .await
            .unwrap();
        assert_eq!((turn.reply.as_str(), turn.run.first_seq), ("again", 4));
    }

    /// A memory whose appends panic, standing in for a bug inside a turn.
    struct PanicsOnAppend(SqliteMemory);

    impl ConversationMemory for PanicsOnAppend {
        fn load<'a>(
            &'a self,
            id: &'a str,
        ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
            self.0.load(id)
        }

        fn append<'a>(
            &'a self,
            _: &'a str,
            _: Vec<Message>,
        ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            Box::pin(async { panic!("bug in the turn") })
        }

        fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
            self.0.clear(id)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_turn_reaches_its_caller_and_releases_the_session() {
        let (service, store, _) = service();
        let service = Arc::new(service);
        let (user, session) = with_session(&service).await;
        let memory = || PanicsOnAppend(service.memory());
        assert!(memory().clear(&session.id).await.is_err());
        let (blocking, _) = agent_with(memory(), [MockTurn::text("a")]);
        let (streaming, _) = stream_agent_with(
            memory(),
            [vec![
                MockStreamEvent::text("b"),
                MockStreamEvent::final_response(usage(1)),
            ]],
        );

        let detached = std::panic::AssertUnwindSafe(service.send_detached(
            Arc::new(blocking),
            &user,
            &session.id,
            "hi",
        ))
        .catch_unwind()
        .await;
        let streamed = service
            .send_stream(streaming, &user, &session.id, "hi")
            .await
            .unwrap();
        let events = drain(streamed).await;

        assert!(detached.is_err());
        // The deltas arrived, then the stream ended without Done. The
        // transport has to report that itself.
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0], TurnEvent::Text(t) if t == "b"));
        service.idle().await;
        // Neither panic left the session locked, and neither saved anything.
        let (agent, _) = agent_with(service.memory(), [MockTurn::text("fine")]);
        let turn = service.send(&agent, &user, &session.id, "hi").await;
        assert_eq!(turn.unwrap().reply, "fine");
        assert_eq!(run_rows(&store).await.len(), 1);
    }
}
