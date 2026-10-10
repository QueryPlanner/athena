//! Reminders: messages and tasks the user schedules for later, as tools.
//!
//! A reminder is a row in `jobs`. `notify` sends its text back to the user
//! at the time; `agent_task` runs its prompt through the agent in the
//! user's current session and sends the reply. The Telegram process
//! delivers them ([`crate::scheduler`]); the tools here only write rows.
//!
//! Times are the user's wall clock ([`Store::timezone`]). A one-off
//! reminder is stored as the instant it names when created. A repeating one
//! is a fixed time of day, daily or on given weekdays, and each next run is
//! computed in the user's zone when the previous one is done, so it follows
//! daylight saving and a later `timezone_set`.
//!
//! An `agent_task` runs a prompt with the user's authority, every day if it
//! repeats, so the model alone cannot schedule one: `reminder_create` only
//! stores it `pending` with a confirmation code and returns a preview, and
//! `reminder_confirm` activates it only in a turn whose user message
//! ([`UserText`]) contains the code and the task's id. A page or file the
//! model reads can make it ask, but only the user can answer. A scheduled
//! turn has an empty [`UserText`], so a task cannot confirm another.
use crate::runner::{Conversation, UserText};
use crate::{store::Store, timezone::for_owner};
use anyhow::{Result, anyhow, bail, ensure};
use jiff::{
    SignedDuration, Timestamp, Zoned,
    civil::{DateTime, Time, Weekday},
    tz::TimeZone,
};
use rig_agent::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Active reminders one user may have, of both kinds.
pub const MAX_ACTIVE: i64 = 50;
/// Active `agent_task` reminders one user may have.
pub const MAX_ACTIVE_TASKS: i64 = 10;
/// `agent_task` runs one user may start in any 24 hours. Past this, a due
/// task is skipped and the user is told.
pub const TASK_RUNS_PER_DAY: i64 = 20;
/// Reminders one user may create in any 24 hours, cancelled ones included,
/// so a task that schedules more tasks cannot flood the table.
pub const MAX_CREATED_PER_DAY: i64 = 100;
/// Longest `notify` text, in bytes.
pub const MAX_TEXT: usize = 1000;
/// Longest `agent_task` prompt, in bytes.
pub const MAX_PROMPT: usize = 2000;
/// Furthest ahead a reminder can be first due.
pub const MAX_AHEAD: SignedDuration = SignedDuration::from_hours(24 * 366);

/// How long a task's confirmation code works.
pub const CONFIRM_WINDOW: SignedDuration = SignedDuration::from_mins(10);

pub const NAMES: [&str; 4] = [
    ReminderCreate::NAME,
    ReminderConfirm::NAME,
    ReminderList::NAME,
    ReminderCancel::NAME,
];

/// What a reminder does when it is due.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Send the stored text.
    Notify,
    /// Run the stored prompt as a turn and send the reply.
    AgentTask,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Notify => "notify",
            Kind::AgentTask => "agent_task",
        }
    }

    /// The kind stored as `name`, if this build knows it.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "notify" => Some(Kind::Notify),
            "agent_task" => Some(Kind::AgentTask),
            _ => None,
        }
    }
}

/// How often a reminder repeats.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Once,
    Daily,
    Weekly,
}

/// A repeating reminder's schedule: a wall-clock time, every day or on
/// some weekdays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recurrence {
    Daily(Time),
    /// The weekdays are sorted Monday first and distinct.
    Weekly(Vec<Weekday>, Time),
}

const WEEKDAYS: [(&str, Weekday); 7] = [
    ("mon", Weekday::Monday),
    ("tue", Weekday::Tuesday),
    ("wed", Weekday::Wednesday),
    ("thu", Weekday::Thursday),
    ("fri", Weekday::Friday),
    ("sat", Weekday::Saturday),
    ("sun", Weekday::Sunday),
];

fn weekday(name: &str) -> Result<Weekday> {
    let lower = name.trim().to_ascii_lowercase();
    WEEKDAYS
        .iter()
        .find(|(short, _)| lower.len() >= 3 && lower.starts_with(short))
        .filter(|(short, day)| {
            let full = format!("{day:?}").to_ascii_lowercase();
            lower == *short || lower == full
        })
        .map(|(_, day)| *day)
        .ok_or_else(|| {
            anyhow!("`{name}` is not a weekday; use mon, tue, wed, thu, fri, sat or sun")
        })
}

fn short(day: Weekday) -> &'static str {
    WEEKDAYS[day.to_monday_zero_offset() as usize].0
}

/// A wall-clock time `HH:MM`, 24-hour.
pub fn time_of_day(value: &str) -> Result<Time> {
    let b = value.as_bytes();
    ensure!(
        b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()),
        "time must be HH:MM, 24-hour, such as 07:30 or 21:00"
    );
    let (hour, minute): (i8, i8) = (value[..2].parse()?, value[3..].parse()?);
    Time::new(hour, minute, 0, 0)
        .map_err(|_| anyhow!("time must be a real time between 00:00 and 23:59"))
}

impl Recurrence {
    /// The schedule as stored in `jobs.recurrence`: `daily@HH:MM` or
    /// `weekly:mon,wed@HH:MM`.
    pub fn to_stored(&self) -> String {
        match self {
            Recurrence::Daily(at) => format!("daily@{}", at.strftime("%H:%M")),
            Recurrence::Weekly(days, at) => format!(
                "weekly:{}@{}",
                days.iter().map(|d| short(*d)).collect::<Vec<_>>().join(","),
                at.strftime("%H:%M")
            ),
        }
    }

    /// Read back what [`Recurrence::to_stored`] wrote.
    pub fn parse(stored: &str) -> Result<Self> {
        let bad = || anyhow!("unknown recurrence `{stored}`");
        let (rule, at) = stored.split_once('@').ok_or_else(bad)?;
        let at = time_of_day(at)?;
        match rule.split_once(':') {
            None if rule == "daily" => Ok(Recurrence::Daily(at)),
            Some(("weekly", days)) => {
                Self::weekly(&days.split(',').map(str::to_string).collect::<Vec<_>>(), at)
            }
            _ => Err(bad()),
        }
    }

    fn weekly(names: &[String], at: Time) -> Result<Self> {
        let mut days = names
            .iter()
            .map(|n| weekday(n))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !days.is_empty(),
            "a weekly reminder needs at least one weekday"
        );
        days.sort_by_key(|d| d.to_monday_zero_offset());
        days.dedup();
        Ok(Recurrence::Weekly(days, at))
    }

    fn time(&self) -> Time {
        match self {
            Recurrence::Daily(at) | Recurrence::Weekly(_, at) => *at,
        }
    }

    fn includes(&self, day: Weekday) -> bool {
        match self {
            Recurrence::Daily(_) => true,
            Recurrence::Weekly(days, _) => days.contains(&day),
        }
    }

    /// The first run strictly after `after`, on the wall clock of `zone`.
    ///
    /// A time that a clock change skips runs at the same distance after the
    /// change (02:30 becomes 03:30); a time that happens twice runs the
    /// first time. Only one run is ever returned, however long `after` is
    /// past the previous one: missed runs are not made up.
    pub fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Result<Timestamp> {
        let mut day = after.to_zoned(zone.clone()).date();
        // A weekly schedule recurs within eight days of any date.
        for _ in 0..=8 {
            if self.includes(day.weekday()) {
                let at = zone.to_timestamp(day.to_datetime(self.time()))?;
                if at > after {
                    return Ok(at);
                }
            }
            day = day.tomorrow()?;
        }
        bail!("no run of {} within eight days", self.to_stored())
    }

    /// For the user: `daily at 09:00`, `weekly on mon, thu at 18:30`.
    pub fn describe(&self) -> String {
        match self {
            Recurrence::Daily(at) => format!("daily at {}", at.strftime("%H:%M")),
            Recurrence::Weekly(days, at) => format!(
                "weekly on {} at {}",
                days.iter()
                    .map(|d| short(*d))
                    .collect::<Vec<_>>()
                    .join(", "),
                at.strftime("%H:%M")
            ),
        }
    }
}

/// `reminder_create`'s arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub kind: Kind,
    /// The message to send (`notify`) or the prompt to run (`agent_task`).
    pub text: String,
    #[serde(default)]
    pub repeat: Repeat,
    /// Once: the local date and time, `YYYY-MM-DDTHH:MM`.
    pub at: Option<String>,
    /// Once: this many minutes from now, instead of `at`.
    pub in_minutes: Option<i64>,
    /// Daily and weekly: the local time of day, `HH:MM`.
    pub time: Option<String>,
    /// Weekly: the days, `mon` to `sun`.
    pub weekdays: Option<Vec<String>>,
}

/// Nonblank, at most `max` bytes, and no control characters but line breaks.
fn text(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty()
            && value.len() <= max
            && !value.chars().any(|c| c.is_control() && c != '\n'),
        "text must be nonblank, printable, and at most {max} bytes"
    );
    Ok(())
}

impl Create {
    /// Check the arguments, and work out the schedule and the first run
    /// from `now` on the wall clock of `zone`.
    pub fn schedule(
        &self,
        now: Timestamp,
        zone: &TimeZone,
    ) -> Result<(Option<Recurrence>, Timestamp)> {
        match self.kind {
            Kind::Notify => text(&self.text, MAX_TEXT)?,
            Kind::AgentTask => text(&self.text, MAX_PROMPT)?,
        }
        let once = self.at.is_some() || self.in_minutes.is_some();
        let repeating = self.time.is_some() || self.weekdays.is_some();
        let (recurrence, first) = match self.repeat {
            Repeat::Once => {
                ensure!(
                    !repeating,
                    "time and weekdays are for daily and weekly reminders; give at or in_minutes"
                );
                let first = match (&self.at, self.in_minutes) {
                    (Some(at), None) => {
                        let local: DateTime = at.parse().map_err(|_| {
                            anyhow!("at must be a local date and time YYYY-MM-DDTHH:MM, such as 2026-10-11T09:00")
                        })?;
                        let resolved = zone.to_ambiguous_timestamp(local);
                        ensure!(
                            !matches!(resolved.offset(), jiff::tz::AmbiguousOffset::Gap { .. }),
                            "{at} does not exist on the user's clock: it is skipped by a \
                             daylight saving change; choose another time"
                        );
                        // A time that happens twice is its first occurrence.
                        resolved.compatible()?
                    }
                    (None, Some(minutes)) => {
                        ensure!(minutes >= 1, "in_minutes must be at least 1");
                        let ahead = SignedDuration::from_mins(minutes.min(MAX_AHEAD.as_mins() + 1));
                        now.checked_add(ahead)?
                    }
                    _ => bail!("give exactly one of at or in_minutes"),
                };
                (None, first)
            }
            Repeat::Daily | Repeat::Weekly => {
                ensure!(
                    !once,
                    "at and in_minutes are for one-off reminders; give time"
                );
                let at = time_of_day(
                    self.time
                        .as_deref()
                        .ok_or_else(|| anyhow!("give time, HH:MM"))?,
                )?;
                let recurrence = match (self.repeat, &self.weekdays) {
                    (Repeat::Daily, None) => Recurrence::Daily(at),
                    (Repeat::Daily, Some(_)) => bail!("weekdays are for weekly reminders"),
                    (_, days) => Recurrence::weekly(days.as_deref().unwrap_or_default(), at)?,
                };
                let first = recurrence.next_after(now, zone)?;
                (Some(recurrence), first)
            }
        };
        ensure!(
            first > now,
            "that time has already passed; reminders must be in the future"
        );
        ensure!(
            first.duration_since(now) <= MAX_AHEAD,
            "reminders can be at most 366 days ahead"
        );
        Ok((recurrence, first))
    }
}

/// When `at` is on the user's wall clock, for the model to repeat back.
pub fn local(at: Timestamp, zone: &TimeZone) -> Value {
    let zoned: Zoned = at.to_zoned(zone.clone());
    json!({
        "datetime": zoned.strftime("%Y-%m-%dT%H:%M%:z").to_string(),
        "weekday": zoned.strftime("%A").to_string(),
        "timezone": crate::timezone::name(zone),
    })
}

/// `reminder_cancel`'s arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub id: i64,
}

pub fn register(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
) -> AgentBuilder<WithBuilderTools> {
    builder
        .tool(ReminderCreate(store.clone()))
        .tool(ReminderConfirm(store.clone()))
        .tool(ReminderList(store.clone()))
        .tool(ReminderCancel(store))
}

pub struct ReminderCreate(pub Store);
impl Tool for ReminderCreate {
    const NAME: &'static str = "reminder_create";
    type Args = Create;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Schedule a reminder for the user, delivered in Telegram. kind notify sends `text` \
         back to them at the time, and is scheduled at once; kind agent_task runs `text` as a \
         task for you at the time, in their current conversation, and sends them your reply, \
         but is only scheduled once the user confirms it (see reminder_confirm). repeat once \
         (default) needs exactly one of `at`, a local date and time YYYY-MM-DDTHH:MM on the \
         user's clock, or `in_minutes`; daily needs `time` HH:MM; weekly needs `time` and \
         `weekdays`. Call now first to resolve words like tomorrow or tonight. Returns the \
         first run on the user's clock: tell it to them."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "kind":{"type":"string","enum":["notify","agent_task"]},
            "text":{"type":"string","description":"The message to send, or the task to do"},
            "repeat":{"type":"string","enum":["once","daily","weekly"]},
            "at":{"type":["string","null"],"description":"Once: local YYYY-MM-DDTHH:MM"},
            "in_minutes":{"type":["integer","null"],"description":"Once: minutes from now"},
            "time":{"type":["string","null"],"description":"Daily or weekly: local HH:MM"},
            "weekdays":{"type":["array","null"],"items":{"type":"string","enum":["mon","tue","wed","thu","fri","sat","sun"]}}
        },"required":["kind","text"],"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, args: Create) -> Result<Value, Self::Error> {
        let session = context.require::<Conversation>()?.0.clone();
        for_owner(&self.0, context, move |store, owner| {
            store.create_reminder(owner, &session, &args, Timestamp::now())
        })
        .await
    }
}

/// `reminder_confirm`'s arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirm {
    pub id: i64,
    pub code: String,
}

pub struct ReminderConfirm(pub Store);
impl Tool for ReminderConfirm {
    const NAME: &'static str = "reminder_confirm";
    type Args = Confirm;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Schedule an agent_task that reminder_create previewed. Call it only after the \
         user's own reply contains the task's id and its confirmation code; it fails \
         otherwise, and nothing runs until it succeeds."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"id":{"type":"integer"},"code":{"type":"string"}},
               "required":["id","code"],"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, args: Confirm) -> Result<Value, Self::Error> {
        let session = context.require::<Conversation>()?.0.clone();
        // Absent, as in a scheduled turn, is the same as nothing typed.
        let said = context
            .get::<UserText>()
            .map(|t| t.0.clone())
            .unwrap_or_default();
        for_owner(&self.0, context, move |store, owner| {
            store.confirm_reminder(
                owner,
                &session,
                args.id,
                &args.code,
                &said,
                Timestamp::now(),
            )
        })
        .await
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

pub struct ReminderList(pub Store);
impl Tool for ReminderList {
    const NAME: &'static str = "reminder_list";
    type Args = NoArgs;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "The user's active reminders, soonest first, each with its id, kind, text, repeat \
         and next run on their clock; the tasks waiting for their confirmation; and the ones \
         that failed to deliver recently."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<Value, Self::Error> {
        for_owner(&self.0, context, |store, owner| {
            store.reminders(owner, Timestamp::now())
        })
        .await
    }
}

pub struct ReminderCancel(pub Store);
impl Tool for ReminderCancel {
    const NAME: &'static str = "reminder_cancel";
    type Args = Cancel;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Cancel one of the user's active reminders, or a task waiting for confirmation, by \
         its id from reminder_list."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"id":{"type":"integer"}},
               "required":["id"],"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, args: Cancel) -> Result<Value, Self::Error> {
        for_owner(&self.0, context, move |store, owner| {
            store.cancel_reminder(owner, args.id, Timestamp::now())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn zone(name: &str) -> TimeZone {
        crate::timezone::parse(name).unwrap()
    }

    fn create(args: Value) -> Create {
        serde_json::from_value(args).unwrap()
    }

    fn local_of(t: Timestamp, z: &str) -> String {
        t.to_zoned(zone(z))
            .strftime("%Y-%m-%d %a %H:%M %:z")
            .to_string()
    }

    #[test]
    fn kinds_round_trip_and_unknown_ones_are_none() {
        for kind in [Kind::Notify, Kind::AgentTask] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("health_sync"), None);
    }

    #[test]
    fn times_of_day_are_strict_hh_mm() {
        assert_eq!(
            time_of_day("07:05").unwrap(),
            Time::new(7, 5, 0, 0).unwrap()
        );
        assert_eq!(
            time_of_day("23:59").unwrap(),
            Time::new(23, 59, 0, 0).unwrap()
        );
        for bad in ["7:05", "07:5", "0705", "07-05", "ab:cd", "07:05:00"] {
            let e = time_of_day(bad).unwrap_err().to_string();
            assert!(e.contains("HH:MM"), "{bad}: {e}");
        }
        for bad in ["24:00", "12:60"] {
            let e = time_of_day(bad).unwrap_err().to_string();
            assert!(e.contains("real time"), "{bad}: {e}");
        }
    }

    #[test]
    fn recurrences_store_parse_and_describe() {
        let nine = Time::new(9, 0, 0, 0).unwrap();
        let weekly = Recurrence::weekly(
            &["fri".into(), "Monday".into(), "mon".into(), " WED ".into()],
            nine,
        )
        .unwrap();
        assert_eq!(weekly.to_stored(), "weekly:mon,wed,fri@09:00");
        assert_eq!(weekly.describe(), "weekly on mon, wed, fri at 09:00");
        assert_eq!(
            Recurrence::parse("weekly:mon,wed,fri@09:00").unwrap(),
            weekly
        );
        let daily = Recurrence::Daily(nine);
        assert_eq!(daily.to_stored(), "daily@09:00");
        assert_eq!(daily.describe(), "daily at 09:00");
        assert_eq!(Recurrence::parse("daily@09:00").unwrap(), daily);
        for bad in ["daily", "hourly@09:00", "daily:mon@09:00", "weekly@09:00"] {
            assert!(Recurrence::parse(bad).is_err(), "{bad}");
        }
        for bad in ["mo", "monday!", "funday", "m"] {
            let e = weekday(bad).unwrap_err().to_string();
            assert!(e.contains("not a weekday"), "{bad}: {e}");
        }
        let e = Recurrence::weekly(&[], nine).unwrap_err().to_string();
        assert!(e.contains("at least one weekday"), "{e}");
    }

    #[test]
    fn the_next_run_is_strictly_after_on_the_users_wall_clock() {
        let daily = Recurrence::parse("daily@09:00").unwrap();
        let kolkata = zone("Asia/Kolkata");
        // 03:00 UTC is 08:30 in Kolkata: today at 09:00.
        let next = daily
            .next_after(at("2026-10-10T03:00:00Z"), &kolkata)
            .unwrap();
        assert_eq!(
            local_of(next, "Asia/Kolkata"),
            "2026-10-10 Sat 09:00 +05:30"
        );
        // Exactly at 09:00 is not after it: tomorrow.
        let again = daily.next_after(next, &kolkata).unwrap();
        assert_eq!(
            local_of(again, "Asia/Kolkata"),
            "2026-10-11 Sun 09:00 +05:30"
        );
        // Days late, still only the next one: nothing is made up.
        let late = daily
            .next_after(at("2026-10-20T10:00:00Z"), &kolkata)
            .unwrap();
        assert_eq!(
            local_of(late, "Asia/Kolkata"),
            "2026-10-21 Wed 09:00 +05:30"
        );

        let weekly = Recurrence::parse("weekly:mon@09:00").unwrap();
        // From Monday 09:00 itself, the next Monday.
        let monday = weekly
            .next_after(at("2026-10-12T03:30:00Z"), &kolkata)
            .unwrap();
        assert_eq!(
            local_of(monday, "Asia/Kolkata"),
            "2026-10-19 Mon 09:00 +05:30"
        );
    }

    #[test]
    fn repeats_keep_their_wall_clock_time_across_daylight_saving() {
        let york = zone("America/New_York");
        let daily = Recurrence::parse("daily@09:00").unwrap();
        // New York leaves daylight saving on 2026-11-01: that day is 25 hours.
        let before = daily.next_after(at("2026-10-31T14:00:00Z"), &york).unwrap();
        assert_eq!(
            local_of(before, "America/New_York"),
            "2026-11-01 Sun 09:00 -05:00"
        );
        assert_eq!(
            before.duration_since(at("2026-10-31T13:00:00Z")).as_hours(),
            25
        );

        // 02:30 does not exist on 2026-03-08 there: it runs at 03:30.
        let skipped = Recurrence::parse("daily@02:30").unwrap();
        let gap = skipped
            .next_after(at("2026-03-08T05:00:00Z"), &york)
            .unwrap();
        assert_eq!(
            local_of(gap, "America/New_York"),
            "2026-03-08 Sun 03:30 -04:00"
        );
        // 01:30 happens twice on 2026-11-01: it runs once, the first time.
        let twice = Recurrence::parse("daily@01:30").unwrap();
        let first = twice.next_after(at("2026-11-01T04:00:00Z"), &york).unwrap();
        assert_eq!(
            local_of(first, "America/New_York"),
            "2026-11-01 Sun 01:30 -04:00"
        );
        let next = twice.next_after(first, &york).unwrap();
        assert_eq!(
            local_of(next, "America/New_York"),
            "2026-11-02 Mon 01:30 -05:00"
        );
    }

    #[test]
    fn a_schedule_that_never_comes_is_an_error() {
        // Only reachable with an empty weekly list, which parsing refuses.
        let never = Recurrence::Weekly(vec![], Time::midnight());
        let e = never
            .next_after(at("2026-10-10T00:00:00Z"), &zone("UTC"))
            .unwrap_err();
        assert!(e.to_string().contains("no run of"), "{e}");
    }

    #[test]
    fn one_off_reminders_take_a_local_time_or_a_delay() {
        let now = at("2026-10-10T03:00:00Z");
        let kolkata = zone("Asia/Kolkata");
        let (repeat, first) = create(json!({"kind":"notify","text":"tea","at":"2026-10-10T17:00"}))
            .schedule(now, &kolkata)
            .unwrap();
        assert_eq!(repeat, None);
        assert_eq!(first, at("2026-10-10T11:30:00Z"));
        let (_, first) = create(json!({"kind":"agent_task","text":"check","in_minutes":90}))
            .schedule(now, &kolkata)
            .unwrap();
        assert_eq!(first, at("2026-10-10T04:30:00Z"));
        let (repeat, first) = create(json!({"kind":"notify","text":"x","repeat":"weekly",
                "time":"08:00","weekdays":["sun"]}))
        .schedule(now, &kolkata)
        .unwrap();
        assert_eq!(repeat.unwrap().to_stored(), "weekly:sun@08:00");
        assert_eq!(
            local_of(first, "Asia/Kolkata"),
            "2026-10-11 Sun 08:00 +05:30"
        );
        let (repeat, _) =
            create(json!({"kind":"notify","text":"x","repeat":"daily","time":"08:00"}))
                .schedule(now, &kolkata)
                .unwrap();
        assert_eq!(repeat, Some(Recurrence::parse("daily@08:00").unwrap()));
    }

    #[test]
    fn bad_schedules_are_refused_with_a_reason() {
        let now = at("2026-10-10T03:00:00Z");
        let york = zone("America/New_York");
        let long = |n: usize| "x".repeat(n);
        for (args, reason) in [
            (
                json!({"kind":"notify","text":"  ","in_minutes":5}),
                "nonblank",
            ),
            (
                json!({"kind":"notify","text":"a\u{7}b","in_minutes":5}),
                "printable",
            ),
            (
                json!({"kind":"notify","text":long(MAX_TEXT + 1),"in_minutes":5}),
                "1000 bytes",
            ),
            (
                json!({"kind":"agent_task","text":long(MAX_PROMPT + 1),"in_minutes":5}),
                "2000 bytes",
            ),
            (
                json!({"kind":"notify","text":"x"}),
                "exactly one of at or in_minutes",
            ),
            (
                json!({"kind":"notify","text":"x","at":"2026-10-11T09:00","in_minutes":5}),
                "exactly one",
            ),
            (
                json!({"kind":"notify","text":"x","in_minutes":0}),
                "at least 1",
            ),
            (
                json!({"kind":"notify","text":"x","at":"tomorrow 9am"}),
                "YYYY-MM-DDTHH:MM",
            ),
            (
                json!({"kind":"notify","text":"x","at":"2026-10-09T09:00"}),
                "already passed",
            ),
            (
                json!({"kind":"notify","text":"x","at":"2028-10-09T09:00"}),
                "366 days",
            ),
            (
                json!({"kind":"notify","text":"x","in_minutes":i64::MAX}),
                "366 days",
            ),
            (
                json!({"kind":"notify","text":"x","at":"2027-03-14T02:30"}),
                "does not exist",
            ),
            (
                json!({"kind":"notify","text":"x","time":"09:00"}),
                "for daily and weekly",
            ),
            (
                json!({"kind":"notify","text":"x","repeat":"daily","in_minutes":5}),
                "for one-off",
            ),
            (
                json!({"kind":"notify","text":"x","repeat":"daily"}),
                "give time",
            ),
            (
                json!({"kind":"notify","text":"x","repeat":"daily","time":"09:00","weekdays":["mon"]}),
                "weekdays are for weekly",
            ),
            (
                json!({"kind":"notify","text":"x","repeat":"weekly","time":"09:00"}),
                "at least one weekday",
            ),
            (
                json!({"kind":"notify","text":"x","repeat":"weekly","time":"9","weekdays":["mon"]}),
                "HH:MM",
            ),
        ] {
            let e = create(args.clone())
                .schedule(now, &york)
                .unwrap_err()
                .to_string();
            assert!(e.contains(reason), "{args}: {e}");
        }
        // A line break is fine in a message.
        let ok = create(json!({"kind":"notify","text":"a\nb","in_minutes":5}));
        assert!(ok.schedule(now, &york).is_ok());
    }

    #[test]
    fn a_time_that_happens_twice_is_its_first_occurrence() {
        let york = zone("America/New_York");
        let (_, first) = create(json!({"kind":"notify","text":"x","at":"2026-11-01T01:30"}))
            .schedule(at("2026-10-10T00:00:00Z"), &york)
            .unwrap();
        assert_eq!(
            local_of(first, "America/New_York"),
            "2026-11-01 Sun 01:30 -04:00"
        );
        assert_eq!(
            local(first, &york),
            json!({"datetime":"2026-11-01T01:30-04:00","weekday":"Sunday",
                   "timezone":"America/New_York"})
        );
    }
}
