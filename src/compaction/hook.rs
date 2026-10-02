//! The context hook: where compaction meets Rig.
//!
//! One [`ContextHook`] is made for each turn and attached to its Rig request
//! (`PromptRequest::add_hook`, `StreamingPromptRequest::add_hook`), so the
//! blocking and the streaming paths compact the same way. This is the only
//! file that depends on Rig's hook API.
//!
//! Before every model call (`on_completion_call`) the hook estimates how big
//! the request is. Above the line it summarizes the oldest messages with a
//! model call and answers with a [`RequestPatch`] whose history is
//! `[summary, ..recent]`. A patch lasts for one model call, so the hook keeps
//! the summary and applies it again to every later call of the turn.
//!
//! What the provider reports for a call arrives in `on_model_turn_finished`,
//! which both paths fire for every call. (`on_completion_response` fires
//! only on the blocking path, and `on_stream_response_finish` only for turns
//! that wrote text.)
//!
//! The hook only decides. It does not write: the checkpoint it chose is left
//! in the hook for `Service::complete` to save once the turn's own rows are
//! in, so a failed turn leaves none. A summary that fails or takes too long
//! is recorded and skipped; it never fails the turn.

use super::{
    Compactor, Reported, SummaryRun, View, choose_cut, estimate, summary_budget, summary_limit,
    summary_message, tokens_in, transcript,
};
use crate::store::{Checkpoint, Store, now_millis};
use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, HookContext, ModelTurnAction,
    ModelTurnFinished, RequestPatch,
};
use rig_core::completion::Usage;
use rig_core::message::Message;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// What a turn's hook decided, for the service to record.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The newest summary the turn made, to save if the turn is saved.
    pub checkpoint: Option<Checkpoint>,
    /// Every summary call the turn made, successful or not.
    pub summaries: Vec<SummaryRun>,
}

/// The summary a turn applies to its requests, and what it stands for.
struct Applied {
    /// The first message kept: the summary replaces the ones before it.
    cut: usize,
    summary: Message,
}

/// How an index into a request's messages becomes a `messages.seq`.
struct Seqs {
    /// The seq of each message the memory loaded.
    loaded: Vec<i64>,
    /// The seq the turn's first message is saved at; the turn's messages
    /// follow the loaded ones, in order.
    next: i64,
    /// Whether the first loaded message is already a summary.
    summarized: bool,
}

impl Seqs {
    fn seq_of(&self, index: usize) -> i64 {
        match self.loaded.get(index) {
            Some(seq) => *seq,
            None => self.next + (index - self.loaded.len()) as i64,
        }
    }
}

#[derive(Default)]
struct State {
    started: bool,
    /// `None` when the loaded messages cannot be matched to rows, so nothing
    /// could be saved: the turn is not compacted.
    seqs: Option<Seqs>,
    /// How many messages the request being sent holds.
    sent: usize,
    reported: Option<Reported>,
    applied: Option<Applied>,
    checkpoint: Option<Checkpoint>,
    summaries: Vec<SummaryRun>,
    /// A summary was just put to use, and the provider has not yet said what
    /// the smaller request costs.
    after_compaction: bool,
}

struct Shared {
    compactor: Arc<Compactor>,
    store: Store,
    session_id: String,
    state: Mutex<State>,
}

/// Compacts one turn of one session. Cheap to clone; clones share the turn's
/// state, which is how the service reads what the turn decided.
#[derive(Clone)]
pub struct ContextHook(Arc<Shared>);

impl std::fmt::Debug for ContextHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextHook")
            .field("session", &self.0.session_id)
            .finish_non_exhaustive()
    }
}

/// What a summary has to cover, found by [`ContextHook::plan`].
struct Plan {
    cut: usize,
    through_seq: i64,
    /// The text the summarizer is given.
    transcript: String,
    /// The most tokens it is asked to write, and the most it may cost to be used.
    budget: u64,
    limit: u64,
}

impl ContextHook {
    /// A hook for one turn in `session_id`. The turn's memory must be the
    /// store's own: the hook reads what it loaded from the store's receipt.
    pub fn new(compactor: Arc<Compactor>, store: Store, session_id: &str) -> Self {
        Self(Arc::new(Shared {
            compactor,
            store,
            session_id: session_id.to_string(),
            state: Mutex::default(),
        }))
    }

    /// What the turn decided so far, leaving the hook empty. Call after the
    /// turn.
    pub fn finish(&self) -> Outcome {
        let mut state = self.state();
        Outcome {
            checkpoint: state.checkpoint.take(),
            summaries: std::mem::take(&mut state.summaries),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock is held leaves nothing half-written that
        // matters: every field is replaced whole.
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// First call of a turn: what the previous run left the context at, and
    /// how this turn's messages map to rows.
    async fn start(&self, view: &View<'_>) {
        let first = !std::mem::replace(&mut self.state().started, true);
        if first {
            let id = self.0.session_id.clone();
            let last = match self.0.store.call(move |s| s.last_prompt_tokens(&id)).await {
                Ok(tokens) => tokens.filter(|t| *t > 0),
                Err(e) => {
                    tracing::warn!("reading the last prompt size failed: {e:#}; estimating it");
                    None
                }
            };
            let receipt = self.0.store.peek_receipt(&self.0.session_id);
            let mapped = receipt.loaded.len() == view.history.len();
            if !mapped || receipt.loaded_next.is_none() {
                tracing::warn!("compaction off: the history is not the store's rows");
            }
            let mut state = self.state();
            // The last request held every message but the previous reply and
            // the new prompt.
            state.reported = last.map(|tokens| Reported {
                tokens,
                counted: view.len().saturating_sub(2),
            });
            state.seqs = receipt.loaded_next.filter(|_| mapped).map(|next| Seqs {
                loaded: receipt.loaded,
                next,
                summarized: receipt.loaded_summary,
            });
        }
        self.state().sent = view.len();
    }

    fn estimate(&self, view: &View<'_>) -> u64 {
        let state = self.state();
        let summary = state.applied.as_ref().map(|a| (&a.summary, a.cut));
        estimate(state.reported, summary, view)
    }

    /// Where to cut, if a cut would help and is allowed. A cut that cannot
    /// be made holds compaction off until the request has grown.
    fn plan(&self, view: &View<'_>, estimate: u64, window: u64, threshold: u64) -> Option<Plan> {
        let compactor = &self.0.compactor;
        if compactor.held_off(&self.0.session_id, estimate, window) {
            return None;
        }
        let state = self.state();
        let seqs = state.seqs.as_ref()?;
        // Messages before `start` are already in the applied summary; the
        // first `covered` are not worth summarizing again (a summary loaded
        // from an earlier turn is one message, and is summarized again).
        let (start, covered) = match (&state.applied, seqs.summarized) {
            (Some(applied), _) => (applied.cut, applied.cut),
            (None, true) => (0, 1),
            (None, false) => (0, 0),
        };
        let cut = choose_cut(view, covered + 1, window / 5)
            .filter(|cut| summary_limit(window) + cut.kept_tokens < threshold);
        let Some(cut) = cut else {
            compactor.hold_off(&self.0.session_id, estimate);
            return None;
        };
        let through_seq = seqs.seq_of(cut.index - 1);
        // What is already summarized is passed on as the summary, followed
        // by what has been said since.
        let earlier = state.applied.as_ref().map(|a| &a.summary);
        let said = view.from(start).take(cut.index - start);
        Some(Plan {
            cut: cut.index,
            through_seq,
            transcript: transcript(earlier.into_iter().chain(said)),
            budget: summary_budget(window),
            limit: summary_limit(window),
        })
    }

    /// Summarize what `plan` says and, if that worked, start using it.
    async fn compact(&self, plan: Plan, estimate: u64) {
        let compactor = &self.0.compactor;
        let model = compactor.settings().model.clone();
        let started_at = now_millis();
        let called = tokio::time::timeout(
            compactor.timeout(),
            compactor
                .summarizer()
                .summarize(&plan.transcript, plan.budget),
        )
        .await;
        let outcome = match called {
            Err(_) => Err(format!(
                "the summary took longer than {} seconds",
                compactor.timeout().as_secs_f64()
            )),
            Ok(Err(why)) => Err(why),
            Ok(Ok(summary)) if summary.text.is_empty() => Err("the summary was empty".into()),
            // The cut was judged safe for a summary of this size. A longer one
            // is not cut short (the end of a summary is where the open tasks
            // are) but not used: the turn goes on as if it had failed.
            Ok(Ok(summary)) if tokens_in(&summary.text) > plan.limit => Err(format!(
                "the summary was about {} tokens, over the {} tokens it may take",
                tokens_in(&summary.text),
                plan.limit
            )),
            Ok(Ok(summary)) => Ok(summary),
        };
        let mut state = self.state();
        state.summaries.push(SummaryRun {
            started_at,
            ended_at: now_millis(),
            model: model.clone(),
            outcome: outcome.as_ref().map(|s| s.usage).map_err(Clone::clone),
        });
        match outcome {
            Ok(summary) => {
                state.applied = Some(Applied {
                    cut: plan.cut,
                    summary: summary_message(&summary.text),
                });
                state.checkpoint = Some(Checkpoint {
                    through_seq: plan.through_seq,
                    summary: summary.text,
                    model,
                    input_tokens: summary.usage.input_tokens as i64,
                    output_tokens: summary.usage.output_tokens as i64,
                    created_at: now_millis(),
                });
                // What the provider last reported was for a bigger request.
                state.reported = None;
                state.after_compaction = true;
                compactor.release(&self.0.session_id);
            }
            Err(why) => {
                tracing::warn!("compacting the session failed, carrying on without: {why}");
                compactor.hold_off(&self.0.session_id, estimate);
            }
        }
    }

    /// The patch that shows the model the summary instead of what it covers,
    /// if the turn has made one.
    fn patch(&self, history: &[Message]) -> CompletionCallAction {
        match &self.state().applied {
            Some(applied) => CompletionCallAction::patch(
                RequestPatch::new().history(
                    std::iter::once(applied.summary.clone())
                        .chain(history.iter().skip(applied.cut).cloned()),
                ),
            ),
            None => CompletionCallAction::continue_run(),
        }
    }

    async fn before_call(&self, history: &[Message], prompt: &Message) -> CompletionCallAction {
        let view = View { history, prompt };
        self.start(&view).await;
        let compactor = &self.0.compactor;
        let estimate = self.estimate(&view);
        if let Some(window) = compactor.window_for(estimate).await {
            let threshold = compactor.threshold(window);
            if estimate > threshold
                && let Some(plan) = self.plan(&view, estimate, window, threshold)
            {
                self.compact(plan, estimate).await;
            }
        }
        self.patch(history)
    }

    fn record_usage(&self, usage: Usage) {
        let mut state = self.state();
        state.reported = (usage.input_tokens > 0).then_some(Reported {
            tokens: usage.input_tokens,
            counted: state.sent,
        });
        // If the smaller request is still over the line, cutting again at
        // once would only summarize the summary: wait until it has grown.
        if state.after_compaction && usage.input_tokens > 0 {
            state.after_compaction = false;
            self.0
                .compactor
                .hold_off(&self.0.session_id, usage.input_tokens);
        }
    }
}

impl AgentHook for ContextHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        self.before_call(event.history, event.prompt).await
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        self.record_usage(event.usage);
        ModelTurnAction::continue_run()
    }
}

#[cfg(test)]
mod tests;
