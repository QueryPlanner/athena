# athena

A template for persistent, tool-using agents in Rust.
rig-agent 0.42 + SQLite, talking to OpenRouter.

The same material as a browsable site is in [`docs/`](docs/README.md)
(`cd docs && npm install && npm run dev`).

## Make a new agent

Edit `src/agent.rs`. That is the only file that changes.

    #[rig_tool(description = "...")]        // add your tools
    fn my_tool(arg: String) -> Result<String, rig::tool::ToolExecutionError> { ... }

    pub const PREAMBLE: &str = "...";       // set the system prompt
    pub const DEFAULT_MODEL: &str = "...";  // set the model

    pub fn configure_with(builder, sandboxes) -> Agent {
        builder ... .tool(MyTool) ...       // register them
    }

`configure_with` defines the agent without persistent tools (`configure` also
omits sandbox tools). `configure_persistent` adds native calorie, workout,
time and reminder tools (and `web_search` when given a `WebSearch`) using
the supplied `Store`. Production wraps it around the OpenRouter client;
tests can wrap it around Rig's mock model. Give the builder conversation
memory first (`builder.memory(service.memory())`) and use the same store for
persistent tools.

Everything else — users, sessions, storage, the turn loop, the CLI — stays
as is.

## Instructions and skills

To change how the agent behaves without editing Rust, point two optional
settings at files:

| Variable | Default | Meaning |
|---|---|---|
| `ATHENA_INSTRUCTIONS` | unset | Path of a text file (at most 16 KiB). Its text is added to the system prompt after the built-in preamble. |
| `ATHENA_SKILLS_DIR` | unset | Path of a directory of [Agent Skills](https://agentskills.io/specification): `<name>/SKILL.md`, each with `name` and `description` front matter. |

Skills are listed in the system prompt by name and description, and the
model calls the `read_skill` tool to load one when a task matches. A skill
you write for Claude Code or Codex works here, as far as `SKILL.md` goes:
only that file is read, never a skill's `scripts/`, `references/` or
`assets/`.

Both are read once, when the agent starts; restart to pick up edits. Nothing
here stops the agent from starting. A missing path, an unreadable file or an
invalid skill is a warning in the log and is left out. Limits: a skill
file is at most 64 KiB, the skills listing at most 16 KiB, and at most 64
skills load (skills past a limit are not loaded). The spec's `name` and
`description` rules are enforced, and a symlink that leads out of the skills
directory is refused. See
[Instructions and skills](docs/content/docs/instructions-and-skills.mdx) for
the details.

Treat both like code. The model follows them with the authority of its own
preamble, so a skill from someone else is instructions you have chosen to
run. They are local files only, and Athena never fetches one. They are part of the system prompt, which telemetry exports with
every other prompt (see Observability), so put no secrets in them.

### The user's own skills

Separately from the owner's skills, each user can keep skills of their own,
stored in `ATHENA_DB` under their user and shared by their linked Telegram and
API identities. They are never listed in the system prompt: the agent reaches
them through six tools, and the prompt only says they exist.

| Tool | What it does |
|---|---|
| `skill_list` | the user's skills: name, description, origin, source and file count |
| `skill_read(name, file?)` | a skill's instructions, or one of its files, fenced and labelled with where it came from |
| `skill_install(repo, path, ref)` | preview a skill from a public GitHub repository; saves nothing |
| `skill_create(name, description, body)` | preview a skill written in the conversation; saves nothing |
| `skill_confirm(preview_id)` | save a previewed skill |
| `skill_remove(name)` | remove one |

Adding a skill takes two calls. `skill_install` or `skill_create` saves
nothing: it returns a preview (name, description, source repository and pinned
commit, files and sizes, an excerpt of the instructions) and a short-lived
`preview_id` (10 minutes, one use, tied to your user and session). The agent
shows you the preview and asks. Only after you agree in the chat does it call
`skill_confirm(preview_id)`, which saves exactly what was previewed.

Athena does not check that you agreed: it relies on the model asking you. A
web page, file or skill that talks the model into confirming could add a skill
without you. The preview, expiry, size limits and the untrusted labelling of
GitHub text limit the damage; they do not remove the risk. Read the preview
before you say yes (see [Known limits](docs/content/docs/known-limits.mdx)).

A skill from GitHub is read on the host through GitHub's public API, without
credentials (so public repositories only, and 60 requests an hour), at the
commit its ref resolved to. Its `SKILL.md` follows the same rules as the
owner's skills, and its other text files are kept (at most 32, 256 KiB in all;
binary and oversized files are skipped). `skill_read` labels it untrusted
third-party content, like a web page. A skill created in a conversation is
labelled as the user's notes. Files are stored, not copied into the sandbox.
A user keeps at most 64 skills.

## Run

    cp .env.example .env                     # then fill in OPENROUTER_API_KEY

    cargo run                                # REPL, session "default"
    cargo run -- research                    # REPL, session "research"
    cargo run -- research "some prompt"      # one-shot, same session
    cargo run -- sessions                    # list your sessions
    cargo run -- sessions new notes          # create an empty session
    cargo run -- usage                       # per-session token totals
    cargo run -- --user telegram:42 ...      # any of the above as another user
    ATHENA_DB=/tmp/x.db cargo run -- ...     # use another database file
    ATHENA_DB=$PWD/agent.db cargo run -- serve   # HTTP API on 127.0.0.1:8080
    ATHENA_DB=$PWD/agent.db cargo run -- serve --addr 127.0.0.1:9000  # or ATHENA_ADDR
    ATHENA_DB=$PWD/agent.db cargo run -- telegram  # bot; TELEGRAM_BOT_TOKEN in .env
    cargo run -- backup backups/today.db     # online copy of the database
    cargo run -- --version                   # athena 0.2.0-dev, or ATHENA_VERSION

`athena` reads `.env` from the directory you run it in. Variables already
set in your shell win over the file, so `ATHENA_DB=/tmp/x.db athena ...`
still works. `.env` is git-ignored; `.env.example` lists every setting. A
malformed `.env` stops `athena` before it touches the database.

`athena serve` and `athena telegram` refuse to start unless `ATHENA_DB` is
an absolute path, so a service never creates a fresh database in whatever
directory it was started from. The REPL and the other commands keep the
default `agent.db` in the working directory.

`athena backup DEST` copies the database (`ATHENA_DB`, else `agent.db`) to
the new file `DEST`, creating its directories. It is safe while `serve` or
`telegram` is running: SQLite's online backup copies a consistent snapshot,
WAL included. It opens the database read-only and never migrates it, so the
binary being replaced can back up a database before its successor migrates
it. It waits up to 30 s for a writer's lock, refuses an existing `DEST`,
never leaves a partial `DEST` behind, and needs no API key.

`athena --version` prints `athena VERSION`: `ATHENA_VERSION` if set (deploys
set it), else the crate version with `-dev`.

| Variable | Used by | Default | Meaning |
|---|---|---|---|
| `OPENROUTER_API_KEY` | anything that talks to the model | none, required | OpenRouter API key |
| `AGENT_MODEL` | all | `openai/gpt-6-luna` | Model id on OpenRouter |
| `ATHENA_DB` | all | `agent.db`; `serve` and `telegram` require an absolute path | SQLite database file |
| `ATHENA_VERSION` | `--version`, `GET /version` | `<crate version>-dev` | The deployed release |
| `RUNS_STORE_RAW` | all | on | `0` drops raw provider responses from `runs.calls_json` |
| `ATHENA_COMPACT_AT` | all | `0.8` | Compact a session once its context is above this share of the model's window, from 0.3 to 0.95; see Compaction |
| `ATHENA_COMPACT_MODEL` | all | `AGENT_MODEL` | The model that writes the summaries |
| `ATHENA_CONTEXT_TOKENS` | all | OpenRouter's `context_length` for the model, else 128000 | The model's context window in tokens, 8000 or more |
| `ATHENA_ADDR` | `serve` | `127.0.0.1:8080` | Listen address; `--addr` wins over it |
| `ATHENA_ALLOWED_HOSTS` | `serve` | unset | Comma list of `HOST` or `HOST:PORT` the API answers; see HTTP API |
| `ATHENA_INSTRUCTIONS` | all | unset | File of instructions added to the system prompt; see Instructions and skills |
| `ATHENA_SKILLS_DIR` | all | unset | Directory of Agent Skills; see Instructions and skills |
| `EXA_API_KEY` | all | unset: no `web_search` tool | Exa API key for the `web_search` tool; see Web search |
| `GOOGLE_HEALTH_CLIENT_ID`, `GOOGLE_HEALTH_CLIENT_SECRET`, `GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY` | all | unset: Google Health off | Set all three or none (some is a startup error); see Google Health |
| `GOOGLE_HEALTH_REDIRECT_URI` | all | `http://127.0.0.1:8080/integrations/google-health/callback` | The redirect URI registered with the Google OAuth client |
| `TELEGRAM_BOT_TOKEN` | `telegram` | none, required | Bot token from @BotFather |
| `TELEGRAM_API_URL` | `telegram` | `https://api.telegram.org` | Bot API server; the tests point it at a fake one |
| `CLOUDFLARE_ACCOUNT_ID` | `telegram` | unset | With `CLOUDFLARE_API_TOKEN`, transcribe voice notes; see Telegram |
| `CLOUDFLARE_API_TOKEN` | `telegram` | unset | Cloudflare API token allowed to run Workers AI |

`athena serve` and `athena telegram` stop the same way on SIGINT (Ctrl-C)
and SIGTERM (`kill`, `docker stop`, systemd): they take no new work, let the
turns in flight finish and reply, then exit 0. In a container, run `athena`
directly (exec-form `CMD ["athena", "serve"]`, or `docker run --init`): a
shell wrapper as PID 1 does not pass SIGTERM on.

A session name is created on first use and resumes the conversation after
that, tool history included. `sessions new NAME` creates one explicitly and
prints `NAME<TAB>ID`; it fails if you already have one by that name.

The CLI acts as the user `cli:local` unless `--user TRANSPORT:ID` says
otherwise. That flag exists so an AI agent (or you) can drive exactly what an
HTTP client or a Telegram chat would see, from a shell. It is not an access
control boundary: anyone who can run the binary can read the database file.

## Local testing

With `OPENROUTER_API_KEY` in your environment or local `.env`, run from the
repository with a separate database. The `target` directory is git-ignored:

```sh
ATHENA_DB="$PWD/target/local-test.db" cargo run --locked -- serve --addr 127.0.0.1:9000
```

In another terminal, check startup:

```sh
curl --fail http://127.0.0.1:9000/health
curl --fail http://127.0.0.1:9000/version
```

Stop with Ctrl-C. This is the HTTP API, with no chat interface. Model turns use
OpenRouter; sandbox tools still require an OpenSandbox server. This command
reuses the testing database on later runs. Keep it on loopback because the API
trusts the caller's user header.

To test the real server without model calls, run:

```sh
cargo build --release --locked
./scripts/smoke-binary.sh target/release/athena
```

The smoke test uses a temporary database and checks HTTP endpoints, session
creation, Host filtering, graceful shutdown and backup.

For faster prototype releases, [direct deployment mode](DEPLOY.md#skip-staging-deployments-for-prototyping)
skips staging deployments while keeping staging installed. Validation and release
tags remain required.

## Telegram

    export TELEGRAM_BOT_TOKEN=123456:ABC...   # from @BotFather
    ATHENA_DB=$PWD/agent.db cargo run -- telegram  # long polling; Ctrl-C stops

`athena telegram` needs `OPENROUTER_API_KEY` and `TELEGRAM_BOT_TOKEN` and
checks both before it opens the database. `TELEGRAM_API_URL` points it at
another Bot API server (the tests use a fake one); it defaults to
`https://api.telegram.org`.

The bot is open to anyone who finds it; see Known limits. It answers private
chats only. A group message is ignored and logged: in a group every member
would read one member's conversation.

Each Telegram user is the user `telegram:<their Telegram user id>`, so
`cargo run -- --user telegram:ID sessions` shows what the bot stored for
them. They talk in one session at a time: `default` until they pick another.
The pick is stored (the `selected_sessions` table), so it survives a restart.

| Command | What it does |
|---|---|
| `/new NAME` | Create session NAME and switch to it. Without NAME, picks `chat-N`. |
| `/sessions` | List your sessions with message counts; `*` marks the current one. |
| `/switch NAME` | Switch to an existing session (`default` always exists). |
| `/usage` | Turns, model calls and tokens per session. |
| `/link API_USER_ID` | Bind a never-used API identity to your existing user and sessions. |
| `/connect_health` | Connect Google Health (read-only): send the link it gives you, then paste back the address Google sends your browser to. See Google Health. |
| `/disconnect_health` | Revoke Google Health access and delete the stored token; synced daily numbers stay. |
| `/help`, `/start` | What the bot is, the current session, the commands. |

The commands are registered with Telegram's command menu at startup. Any
other text is a prompt for the current session.

A photo or a file is a prompt too, with its caption as the text (a caption
is never a command). The bot downloads it (Telegram allows bots up to 20 MB;
a larger file is refused with a reply), saves it in the session's sandbox
under `/tmp/athena-inbox/`, and tells the model its name, type, size and
path. The name is made safe first: only letters, digits, spaces and `._()-`
are kept, anything else becomes `_`. A photo, or a PNG, JPEG, GIF or WebP file, is also shown to the model
as an image. Files the model sends with `send_photo` and `send_file` arrive
after its reply; a photo Telegram refuses (odd proportions, say) is sent
again as a file. The photos of an album arrive as separate messages; the
bot collects them until 2 seconds pass with no new one, then runs one turn
for all of them, with the album's caption as the text. See "Images and
files" below.

A voice note or an audio file is a prompt when `CLOUDFLARE_ACCOUNT_ID` and
`CLOUDFLARE_API_TOKEN` are both set (setting only one stops the bot at
startup). The bot downloads it and sends it to Cloudflare Workers AI's
`@cf/openai/whisper-large-v3-turbo`, and the transcript, after the caption
if there is one, is the prompt. The transcript is not sent back to you; the
reply shows what was understood, and the transcript is stored in the session
like typed text. The audio is never stored. Notes up to 2 MB and 5 minutes
are transcribed, one at a time across all users; a larger or longer one is
refused with a reply, and one that cannot be transcribed is answered "I
could not transcribe that voice note". The token goes to Cloudflare only, as
a header, and is never logged.

While the model works the bot shows "typing...". The model writes Markdown
and the bot renders it for Telegram: bold, italic, strikethrough, `code`,
fenced code with its language, links, blockquotes, headings (bold),
bullet and numbered lists, and tables (an aligned grid in a code block).
It sends plain text plus Telegram's formatting entities, never a parse mode,
so a stray `*` or `_` in a reply stays as written and cannot make Telegram
refuse the message. If Telegram answers "Bad Request" to a formatted
message, that message is sent again as plain text. A reply longer than
Telegram's 4096-character limit is split, at a blank line if there is one
near the limit, else a line break, else a space, never inside a character,
and a code block or a style that crosses the cut is closed and reopened in
the next message; at most 8 messages, then a note that the rest is in the
transcript. A 429 from Telegram is retried after the delay it asks for.

Each turn runs in its own task, so a slow model never holds up another user.
One user gets one turn at a time: a message that arrives while their turn is
running is answered "Still working on your last message" and dropped, not
queued. Commands still answer at once. Errors are logged to stderr and
answered with one short line; the bot keeps running. Ctrl-C stops polling
and waits for turns in flight to reply.

## Calorie tools

Persistent agents expose `calorie_log`, `calorie_history`, `calorie_summary`,
`calorie_update`, and `calorie_remove`. Food entries live in `ATHENA_DB`, owned
by the user behind the current session. Linked Telegram and API sessions share
these entries. No user ID is accepted in tool arguments.

To log food, provide a `meal` containing `description`, `consumed_date`
(`YYYY-MM-DD` in the user's local calendar), and `source` (`user` or `estimated`).
Optional values are `calories` in kcal, `protein_g`, `carbs_g`, `fat_g`, and
`meal_type`. Unknown nutrition remains null. Athena does not contact a food
nutrition service; the agent must distinguish estimates from user-provided values.
If the local date is unclear, ask the user before saving.

Each occurrence needs a fresh opaque `request_key`, such as a UUID. Reuse that
key only to retry the same original meal. Matching retries return the current
entry; a different meal with the same key conflicts. This prevents duplication
only when a retry uses the same key. It does not deduplicate separate agent
turns that choose different keys. Identical meals with different keys remain
separate occurrences.

History and summaries require inclusive `start_date` and `end_date`. History
returns active entries in descending logging-ID order, with a default limit of
20 and maximum of 50. Pass `next_before_id` as `before_id` to read another page.
Summary totals include known values and report missing counts for each nutrient;
a total is null if no value is known.

Updates replace the meal and require `id` plus `expected_version` from history.
Removal requires the same version check and soft-deletes the entry. Missing,
deleted, and stale-version entries return the same conflict message. Read again
before retrying a correction. Removal retains the original retry key, so a
late log retry returns the deleted entry without restoring it.

Schema migration 9 adds these records without importing the legacy `tools.db`.
Library callers can use `agent::configure_persistent` with the same `Store` as
the builder's memory to expose these tools.

## Workout tools

Persistent agents track a Push / Pull / Legs split and a weekly VO2 session
(a rowing piece) for the user behind the current session:

| Tool | What it does |
|---|---|
| `workout_log(request_key, workout)` | logs a session: its exercises with every set (reps, `weight_kg`, optional `target_reps`, `is_warmup`), or a rowing piece |
| `workout_last(day_type, before_date?)` | the previous session of that type, before today by default, with every set and a suggestion per exercise |
| `workout_next()` | today's day in the rotation, whether VO2 is due, what is already logged today, and the previous session of that type |
| `exercise_progress(kind?, exercise?, distance_m?, limit?)` | a lift's top set and estimated 1RM per session, or rowing times and 500 m splits |
| `workout_history(start_date, end_date, limit?, before_id?)` | sessions in a date range, paginated like `calorie_history` |
| `workout_update(id, expected_version, workout)` / `workout_remove(id, expected_version)` | correct or soft-delete a session |

Weights are kilograms only; the agent converts pounds. A rowing piece is its
distance (default 2000 m) and time (`7:05.3`); the split is derived. When the
user starts a training day the preamble has the agent call `workout_next`,
show the previous session of that type, and offer the suggestion it carries:
2.5 kg more when the top set reached its target reps, otherwise one more rep.
"Last time" never means today's own session, and "today" is the user's own
date in their time zone, computed on the server. Exercises match across
sessions ignoring case and spacing, so the agent reuses earlier names.
Retries, versions and soft deletion work as for the calorie tools. Schema
migration 11 adds the `workout_sessions`, `workout_sets` and
`rowing_results` tables.

## Time and time zone tools

The model has no clock. Persistent agents expose two tools, registered with the
calorie tools, that act for the user behind the current session:

| Tool | What it does |
|---|---|
| `now()` | the user's `datetime` (RFC 3339 with offset), `date`, `weekday`, `timezone` and `utc_offset` |
| `timezone_set(timezone)` | sets the user's IANA zone, e.g. `Europe/London`; returns `previous_timezone` and `now` in the new zone |

Every user starts on `Asia/Kolkata`. The preamble tells the agent to call `now`
before reasoning about dates or times, and `timezone_set` when the user says
where they are. Fixed offsets such as `+05:30` and abbreviations such as `IST`
are refused, because they get daylight saving wrong; the agent is told to ask
for a city. Linked Telegram and API identities share one zone. Schema
migration 10 adds the `user_settings` table that holds it. Zones resolve
against the host's `/usr/share/zoneinfo`, or a copy built into the binary
when the host has none. Code that needs the user's "today" calls
`Store::today(owner, jiff::Timestamp::now())`.

## Web search

With `EXA_API_KEY` set, the agent has `web_search(query, num_results)`: it
calls Exa's search API (`POST https://api.exa.ai/search`) from the machine
running athena and returns up to 10 results (default 5), each with its
title, URL, publication date when Exa knows it, and up to three short
highlights. Without the key the tool does not exist, and the name is still
kept from MCP tools.

- The result is text from web pages, so it is wrapped in markers that carry
  a fresh nonce per call and labelled untrusted; the agent is told never to
  follow instructions in it.
- The tool cuts its own result to 64 KiB (`policy::MAX_RESULT_BYTES`): the
  policy hook limits only MCP tools.
- A search gives up after 30 s. Errors name the HTTP status (a refused key,
  the rate limit, rejected parameters) and never contain the key, which is
  sent only as the `x-api-key` header; redirects are not followed.
- Like every tool call, the query and the result are stored in the session
  and recorded on telemetry spans.
- Evals (`configure_custom`) do not have the tool.

On the VM, a human adds `EXA_API_KEY=...` to `/etc/athena/<env>.env` with
`sudoedit` and restarts the units.

## Google Health

With `GOOGLE_HEALTH_CLIENT_ID`, `GOOGLE_HEALTH_CLIENT_SECRET` and
`GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY` set (the values Blacki uses), a Telegram
user can connect their Google Health (the Fitbit successor) read-only, and the
agent gets `health_status`, `health_summary(days)`, `health_sync_now` and, for
the raw points, `health_data_size`, `health_points` and `health_export` (below).
It uses them for sleep, recovery and activity when you ask what to train.
Without the three settings the tools say it is not set up, and setting only
some stops startup, naming the missing ones.

There is no web callback. Send `/connect_health` in the private chat, open the
link and approve, then copy the address the browser ends up on (it may not
load: that is fine) and paste it into the chat. The bot handles that message
itself: it is never sent to the model, saved in the conversation or logged.
You can delete it from the chat afterwards. The refresh token is stored
encrypted; the access token is only ever in memory. `/disconnect_health`
revokes it at Google and deletes it; the daily numbers already synced stay.

It asks for every read-only scope Google offers, so all your data can be
stored: activity and fitness, health metrics (heart rate, HRV, SpO2, glucose,
temperature, weight and more), location, nutrition, sleep, reproductive
health, logged symptoms, mood, ECG and irregular rhythm notifications. Some
are sensitive; untick any you do not want on Google's consent screen, and
those data types are skipped. Anyone connected before this must send
`/connect_health` again. Every data point is stored whole in the database
(`health_points`), and your history is fetched in the background, a few weeks
more each day, back three years. That is a lot of rows (minute-level heart rate
alone is about 525,000 a year): see `plans/contracts.md` for the numbers, and
note `athena backup` includes it. `health_summary` gives per-day totals and
summaries and can be asked for just some `metrics`.

The model reaches the raw points in three steps, as the question needs.
`health_data_size` says how many points of each type there are, from when to
when, and roughly how many bytes. `health_points(type, from, to)` returns a
page of points (at most 500, about 56 KiB), for a workout or a night. For
months of data, `health_export` builds a SQLite file in the conversation's
sandbox (`/tmp/athena-data/health.sqlite`, tables `points` and `meta`, views
`heart_rate`, `steps`, `weight`, `sleep`, `hrv`, `spo2`) straight from the
database, so the points never pass through the model, which then queries the
file with python3 or `run_code` (the sandbox has no `sqlite3` command). It is
refused above 128 MiB of data (narrow the types or dates: minute-level heart
rate passes that at about ten months), only runs when you ask in a message (not
in a daily brief or reminder task), and the file is gone when the sandbox
expires after 30 idle minutes. Points returned by `health_points` go to the
model provider and are stored in the conversation, and an export puts your
health history in a sandbox that has internet access: see Known limits.

`athena telegram` syncs the last 14 days once a day, after 05:30 your time
(`timezone_set`), and `health_sync_now` allows one more an hour. If Google
stops accepting the connection the bot tells you once to send
`/connect_health` again. Google keeps refresh tokens from an OAuth consent
screen in "Testing" status for only 7 days (see Known limits).

On the VM, a human adds the three variables to `/etc/athena/<env>.env` with
`sudoedit`, restarts the units, and each user sends `/connect_health` once.
The redirect URI must be the one registered with the OAuth client.

## Daily brief

Ask in Telegram: "send me a training brief every morning at 6:30". The agent
calls `daily_brief_set` and each morning at that time on your clock
(`timezone_set`) `athena telegram` sends a short message: what is due today
(Push, Pull, Legs, VO2), last session's numbers with what to aim for, your
last three days of sleep, resting heart rate and steps from Google Health
(left out when it is not connected), and yesterday's calories. It waits up to
30 minutes for that morning's Google Health sync. `daily_brief_status` shows
it and `daily_brief_off` stops it. It is at most one message a day, never
made up if more than four hours late or while you are mid-conversation past
that, and it is switched off if Telegram refuses delivery. The brief is a
turn in your current session, so you can reply to it.

## Reminders

Persistent agents also expose `reminder_create`, `reminder_confirm`,
`reminder_list` and `reminder_cancel`, acting for the user behind the current session. A reminder
is one of two kinds:

- `notify`: at the time, the bot sends you the text ("Reminder: water the
  plants").
- `agent_task`: at the time, the agent runs the text as a task in your
  current Telegram session ("every morning at 7:30, summarise the news") and
  sends you its reply, as if you had asked then. Because it acts with your
  authority, it is only scheduled once you confirm it: the agent shows you
  the whole task, when it runs and a code, and you reply `confirm #<id>
  <code>` within 10 minutes (`reminder_confirm` checks your own message for
  both). A page or file the agent reads cannot schedule one.

A reminder runs once (at a local date and time, or in N minutes), daily at a
time, or weekly on given weekdays at a time, on your wall clock: it keeps its
time across daylight saving and follows `timezone_set`. The preamble tells the
agent to call `now` first, turn "tomorrow at 9" into a local time, and repeat
the scheduled time back to you.

Reminders are delivered in Telegram by `athena telegram`, which checks for due
ones every 30 seconds; `athena serve` never sends them. A late run (the bot was
down) is sent once, marked late; missed repeats are not made up. If you are
mid-conversation when a task falls due, it waits for you, for up to 30
minutes. If you block the bot, your reminders stop. Limits: 50 active
reminders, 10 of them tasks, and 20 task runs a day. The full contract is in
`plans/contracts.md`, "Scheduler and reminders".

## Sandbox and browser tools

Tools that touch a computer run in a sandbox on an
[OpenSandbox](https://github.com/opensandbox-group/OpenSandbox) server, never
on the machine running athena. Native calorie, workout, time and reminder
tools access only Athena’s SQLite store. Other host tools are `add`,
`web_search` (see "Web search") and the tools of the MCP servers you list
(see "Tools from MCP servers").

| Tool | What it does |
|---|---|
| `shell(command, timeout_secs?)` | bash in a persistent session: `cd`, exports and venvs carry over |
| `run_code(language, code)` | a persistent Python interpreter (execd's Jupyter-backed code API) |
| `read_file(path)` / `write_file(path, content)` | text files inside the sandbox; reads stop at 64 KiB |
| `agent_browser(args)` | the whole agent-browser CLI, unrestricted: `args` are the arguments after `agent-browser`, run on the conversation's own browser. The description tells the model to read `skills get core` and `--help` first. Output is capped like `shell`'s; there is no confirmation list, so `eval` and `download` run. The model can pass its own `--session`, which starts a separate browser |
| `browser_login_link(url)` | opens a sign-in page in the conversation's browser and returns a link for the user to sign in themselves (see "Signing in to websites") |
| `view_image(path)` | shows the model a PNG, JPEG, GIF or WebP file from the sandbox |
| `send_photo(path, caption?)` / `send_file(path, caption?)` | sends the user a sandbox file after the reply (Telegram only) |

**One sandbox per session.** The first tool call in a session creates it;
later calls reuse it and push its expiry `ATHENA_SANDBOX_TIMEOUT_SECS` into
the future. A sandbox that expired or was deleted is replaced on the next
call (its files are gone). The sandbox id, bash session and interpreter are
kept in the `sandboxes` table, so restarts and other processes find them.
Tools know their session from the run itself (Rig's per-request
`ToolContext`, set in `src/runner.rs`), never from the model.

**Limits.** Each tool result is cut to 16 KiB. A tool call may carry at most 128 KiB of
arguments; a larger one is skipped and the model is told why
(`src/policy.rs`). There is no limit on how many model turns or tool calls
one reply makes, and no way yet to stop a reply that is running. `agent_browser`
runs whatever arguments the model gives, including `eval` and downloads; only
page content is wrapped in agent-browser's content boundaries. Commands run
without a terminal, so interactive programs hang until their timeout.

**Images and files.** The model sees images, not descriptions of them:
screenshots, `view_image`, and photos users send. `src/media.rs` does the
plumbing:

- OpenRouter's chat API takes images only from the user, and Rig refuses to
  send one in a tool result. `media::Vision` wraps the provider's model and
  moves each tool-result image into a user message right after the tool
  results, labelled with the tool's name. A request carries at most 4
  images, the newest; older ones become `[older image omitted]`.
- An image is shown only up to 3.75 MB (5 MB of base64). A larger one is
  described, not shown.
- Images are not stored. The transcript keeps
  `[image not kept in the transcript]` in their place, so a later turn
  never pays for them again. The files are still in the sandbox until it
  expires, and `view_image` shows them again.
- Telemetry exports no image data: every span exporter cuts base64 runs over
  1024 characters to a note of their length (`telemetry::Redacted`). The
  stderr log is not filtered this way. With the default `warn,athena=info`
  it shows none of Rig's spans, but `RUST_LOG=info` or finer prints their
  fields, images included, with every event inside them.
- `send_photo` takes images up to 10 MB and `send_file` any file up to
  50 MB, Telegram's limits; at most 10 files a turn. Over HTTP and the CLI
  there is nowhere to send a file, and both tools tell the model so.

**Signing in to websites.** The user signs in, never the model:

1. When a site needs a sign-in, the agent calls `browser_login_link(url)`. It
   loads the user's saved sign-ins into the conversation's browser, opens
   `url`, and returns a link such as
   `http://100.124.202.79:18080/browser/<token>`, which the agent sends in
   its reply (Telegram or HTTP).
2. The user opens it on any device on the tailnet. `athena serve` lists the
   page's fields and buttons (`snapshot -i`) as real ones: email or username,
   password and one-time-code fields get the `autocomplete` hints that let a
   phone's password manager and SMS-code autofill fill them. Pressing one of
   the page's buttons, or Enter, fills every field and clicks it in one
   command (`fill @eN ...`, `click @eN`), then lists the next page's fields,
   so a 2FA code step works the same way. What a field is for is guessed
   from its accessible name (`sandbox::login::kind`). Below the fields is the
   browser as a screenshot refreshed every second, with a fallback to tap
   it, type into whatever has focus, press keys, scroll and open a URL. All
   of it drives the same agent-browser session the agent uses.
3. **Done** runs `agent-browser state save` and keeps the result (cookies and
   local storage) in `browser_states`, one per user. The user then tells the
   agent in the chat that they are done, and the agent carries on in the
   same browser.
4. Every new sandbox of that user runs `state load` with it before its
   first command, so sign-ins outlive sandboxes. When a saved sign-in has
   expired, the agent sees the site's sign-in page and sends a new link.

Links last an hour (`sandbox::login::LINK_TTL`) and live in `browser_links`,
so the Telegram process makes them and the `serve` process answers them. A
link opened after its sandbox expired reopens its start page in the new one.
The link's base is `ATHENA_PUBLIC_URL`, else `http://$ATHENA_ADDR`. On the
VM that is the tailnet address `ATHENA_ALLOWED_HOSTS` already lists, and
both units read it from the same env file. `serve --addr` does not change
the links; set `ATHENA_PUBLIC_URL` if users reach Athena at another
address. The sign-in routes are in `src/http/viewer.rs`, the rest in
`src/sandbox/login.rs`. What this does not protect against yet is under
Known limits.

**Configuration.** Without `OPEN_SANDBOX_URL` none of these tools exist and
everything else works as before. Photos users send are still shown to the
model. Other files are not downloaded; the model is told their name and
size, and that there is no sandbox.

| Variable | Default | Meaning |
|---|---|---|
| `OPEN_SANDBOX_URL` | unset | e.g. `http://100.118.54.67:9090` |
| `OPEN_SANDBOX_API_KEY` | unset | sent as `OPEN-SANDBOX-API-KEY` when set |
| `ATHENA_SANDBOX_IMAGE` | `ghcr.io/queryplanner/athena-sandbox:latest` | image each sandbox runs |
| `ATHENA_SANDBOX_TIMEOUT_SECS` | `1800` (min 60) | idle lifetime of a sandbox |
| `ATHENA_PUBLIC_URL` | `http://$ATHENA_ADDR` | where sign-in links point; must be http(s), and required when `ATHENA_ADDR` is `0.0.0.0` or `[::]` |

**The image** is `deploy/sandbox-image/Dockerfile`: Debian 13 slim, Chromium,
a pinned and checksummed agent-browser release, Python with a Jupyter server,
and everyday tools: ffmpeg and yt-dlp; git, curl, wget, jq, ripgrep, fd,
file, tree, zip, unzip and xz; dig, ssh and rsync; and the Python libraries
requests, pandas, pillow and beautifulsoup4 (pinned in the Dockerfile).
Build it on (or for) the sandbox host, then check every tool runs:

    docker build -t athena-sandbox:dev deploy/sandbox-image
    scripts/sandbox-image-check.sh athena-sandbox:dev
    ATHENA_SANDBOX_IMAGE=athena-sandbox:dev

CI runs the same check on every PR, and on `main` before pushing the image.
A tool added to the Dockerfile belongs in the check's list too.

To upgrade agent-browser, change `AGENT_BROWSER_VERSION`, both
`AGENT_BROWSER_SHA256_*` digests and `AGENT_BROWSER_SKILLS_SHA512` together.
The last one pins the npm tarball the usage guides come from: the release
binaries do not carry them, and `agent_browser` tells the model to read them.

**Accepted risk, until the sandbox host is hardened** (plan section 4):
sandboxes run as root with internet egress, can reach other tailnet
machines, and the OpenSandbox server accepts requests from any tailnet
device. A prompt injection can therefore make the agent send anything it
has seen to the internet. athena passes none of its own secrets into a
sandbox; keep secrets out of conversations too. `agent_browser` adds to this:
the model can run `eval` on pages that hold the user's signed-in state, no
domain allowlist applies, and it can save cookies and profiles to the
sandbox's disk or start listeners (`dashboard`, `stream`) that other tailnet
machines can reach. The only guard is the prompt's rule to ask before
signing in. Sandboxes are not deleted
when a session is (there is no session delete yet); they expire after the
timeout.

## Tools from MCP servers

`ATHENA_MCP_CONFIG` names a JSON file listing [MCP](https://modelcontextprotocol.io)
servers, in the `mcpServers` shape Claude Code and Claude Desktop use. Their
tools join the agent's own, in any language, without rebuilding athena. Unset
means no MCP tools; athena looks in no default place, so a file in the working
directory cannot add tools to an agent that did not ask.

```json
{
  "mcpServers": {
    "files": {
      "command": "mcp-server-files",
      "args": ["--root", "/srv/notes"],
      "env": {"API_TOKEN": "${FILES_TOKEN}"}
    },
    "wiki": {
      "url": "https://wiki.example/mcp",
      "headers": {"Authorization": "Bearer ${WIKI_TOKEN}"},
      "timeoutSecs": 30
    }
  }
}
```

| Field | Meaning |
|---|---|
| `command`, `args`, `env` | a stdio server: a child process of athena |
| `url`, `headers` | a streamable-HTTP server (an http or https address). The legacy SSE transport is refused |
| `timeoutSecs` | how long one tool call may take; default 120. A call that runs over returns an error the model sees |
| `startupTimeoutSecs` | how long the server may take to start, handshake and list its tools; default 60 (`npx` and `uvx` fetch on first use) |
| `disabled` | `true` leaves the server out, as in Claude Desktop |
| `type` | optional: `stdio`, `http` or `streamable-http`, which must match the entry |

Other fields are ignored.

**Secrets stay out of the file.** `${VAR}` and `${VAR:-default}` in `args`,
`env` values and `headers` values are read from athena's environment, which
`.env` or the VM's env file fills; `$${` is a literal `${`, and a default
ends at the first `}`. A variable that is unset and has no default skips that
server, with a warning that names the variable. Values are never logged. `url`
is not expanded: put keys in `headers`, and use https, since headers go out as
written. Messages that name a URL show only its scheme, host and port. Prefer
`env` to `args`: arguments show in `ps`.

**A stdio server runs on the host, for every user.** It has athena's user's
permissions and does not run in the sandbox, so the rule "everything runs in a
sandbox" holds only while `mcp.json` lists none. One connection, with the
credentials in its `env` and `headers`, serves every user and session of the
process: anyone who may talk to athena may use these tools. List only servers
you trust. A stdio server gets its declared `env` plus `PATH` and `HOME` from
athena, not the rest of the environment (athena's own keys among it), so
`TMPDIR`, `LANG`, proxy and certificate settings must be declared too if it
needs them. Its stderr goes to athena's. When athena stops it closes each
server's stdin, waits a few seconds, then kills and reaps the process it
started; a turn still running that needs a server gets an error, because
servers stop with athena. A program that process starts itself (the one behind
an `npx` wrapper) is not tracked, and if athena is killed outright a server is
expected to exit when its stdin closes.

**Startup.** `athena eval` connects no server, as it uses no sandbox: evals
start no process. Servers connect at startup, together, and `serve` listens only
after they have. One that cannot start, handshake or list its tools is a
warning (`warning: mcp server ...` on the CLI, a `warn` log line for `serve`
and `telegram`) and its tools are missing; athena runs without them. The CLI
connects only when a command runs a turn. A server that dies later fails its
tool calls until athena restarts: there is no reconnect.

**Names and shapes.** A tool keeps the name its server gave it: Rig cannot
rename an MCP tool. A tool whose name an athena tool has (`add`, `read_skill`, the
calorie and user skill tools, and the sandbox tools, whether or not they are set up) or a server earlier in name
order has is skipped with a warning; the official filesystem server's
`read_file` and `write_file` are lost this way. So is a tool a model provider
would refuse, which fails every request: a name of more than 64 characters or
with characters other than letters, digits, `_` and `-`, or an input schema
that is not of type object. A server's tools past the first 100 are skipped,
and a description is cut to 4 KiB.

**Limits.** What a server sends is untrusted data, like a web page: its
descriptions as much as its results. `ToolPolicy` (`src/policy.rs`) applies as
to every tool: arguments over 128 KiB are not sent, an MCP tool's text results
are cut to 64 KiB in all: blocks are kept in order until the budget is spent
(an image or an empty block counts as one byte), the rest are left out, and
one note says so. At most 4 images are kept, none over 3.75 MB. Other images
reach the model as images, as screenshots do. A reply is read whole into
memory before the cut.

The code is `src/mcp.rs`; `tests/mcp.rs` runs a tiny stdio server built in
`tests/mcp/fixture.rs`, so the tests start real child processes and need no
other runtime.

## Users and sessions

A user has one stable numeric `users.id`. The `user_identities` table maps
`(transport, external_id)` identities to that owner: `("cli", "local")`,
`("telegram", "<Telegram user id>")`, and `("http", "<API user id>")`.
Unlinked identities get separate users. The original identity columns in
`users` remain for compatibility; lookup uses `user_identities`.

In a private Telegram chat, send `/link my-api-name` before using that API
identity. Requests with `X-Athena-User: my-api-name` then resolve to your
existing Telegram user. Both channels can list, read and continue the same
sessions. Browser sign-ins are also shared because saved browser state belongs
to the user. The API names a session explicitly; it does not change Telegram's
selected session.

Repeating your own link succeeds. An API identity already assigned to another
user is refused, even if it has no sessions. Existing users, sessions and
messages are never merged or reassigned. Choose a never-used API identity.
Links survive restarts and work across the serve and Telegram processes when
they use the same database.

Linking does not authenticate API callers. Anyone who can reach the API and
provide the linked header can act as that user. This feature retains the
trusted-private-network model; authenticated linking and unlinking are deferred.

A session belongs to exactly one user. Its name is unique among that user's
sessions; its id (a uuid) is unique everywhere. Two users can both have a
session called `default` and never see each other's. A user asking for
another user's session id gets "no such session", the same answer as for an
id that does not exist.

## Deploy

Production is one Linux VM on your Tailscale tailnet running staging and
prod as systemd services, deployed by GitHub Actions: a merge to `main`
deploys staging, and pushing a `v*` tag promotes the same artifact to prod.
There is no approval step: the tag is the release decision, and only repo
admins may create `v*` tags. Each env can run its own Telegram bot. Traces and logs go to OpenObserve and to JSONL files for DuckDB.
Nothing listens on a public interface.

Give this prompt to your coding agent (Claude Code, Codex, Gemini CLI):

> Set up Athena for me. My VM is reachable over Tailscale at `<ip-or-name>` as
> `<user>`. Follow `SETUP.md` exactly: run preflight first, show me the plan and wait
> for my OK before changing anything, and never ask me to paste secrets into this chat.

Or follow [SETUP.md](SETUP.md) yourself; it runs the same scripts:

    ./scripts/preflight.sh --vm <user>@<vm>              # read-only report
    ./scripts/setup-host.sh --vm <user>@<vm> --dry-run   # review, then run without --dry-run
    ./scripts/init-github.sh --dry-run                   # review, then run for real
    ./scripts/doctor.sh --vm <user>@<vm>                 # verify end to end

The VM may already run other things: setup never touches Docker, ufw, sshd,
Caddy or Tailscale settings, and never overwrites a file holding secrets.

## Layout

    src/agent.rs    <- you edit this
    src/service.rs     the core every transport calls: users, sessions, send
    src/store.rs       the database: migrations, Rig conversation memory, runs
    src/runner.rs      the Run trait, the run record, each run's tool context
    src/compaction.rs  compaction: settings, the window, token estimates, where a
                       cut may fall; compaction/hook.rs is the only code that
                       uses Rig's hook API
    src/sandbox.rs     per-session OpenSandbox sandboxes; sandbox/ has the
                       HTTP client, stream parser, quoting and the tools
    src/policy.rs      the tool-call argument-size hook
    src/search.rs      web_search: Exa's search API, when EXA_API_KEY is set
    src/untrusted.rs   nonce markers and the size fit for untrusted tool results
    src/brief.rs       daily training brief: tools, due time, fixed prompt
    src/health.rs      Google Health: settings, token encryption, the pasted
                       callback; health/ has the Google client, the daily
                       aggregation, the sync, the tools and the raw-point export
    src/custom.rs      the owner's instructions file and skills; custom/ has
                       the front matter parser and the skills and read_skill
    src/user_skills.rs each user's own skills and their tools; user_skills/
                       has the GitHub reader
    src/media.rs       images and files: the provider adapter, the outbox,
                       stripping images from transcripts
    src/cli.rs         the CLI transport: arguments, output, REPL
    src/http.rs        the HTTP transport: JSON API, SSE streaming, `serve`
    src/telegram.rs    the Telegram transport: commands, sessions, the bot;
                       telegram/jobs.rs delivers reminders
    src/reminders.rs   the reminder tools and their schedules
    src/scheduler.rs   the loop that claims and runs due jobs
    src/ops.rs         deployment: version, online backup, absolute ATHENA_DB
    src/telemetry.rs   tracing and OpenTelemetry; telemetry/ has the JSONL
                       files exporter
    src/eval/          `athena eval`: cases, cassettes, graders, judge, results
    src/bench.rs       `athena bench`: load check against a running server
    src/main.rs        wiring: real database, provider, stdin/stdout
    src/gate/          deploy-gate, the only program CI runs on the VM (DEPLOY.md)
    src/bin/           deploy-gate's wiring
    evals/             eval cases and their recorded cassettes
    tests/             integration tests, upgrade fixtures, schema snapshot
    scripts/           coverage gate, live end-to-end test, VM and GitHub setup
    deploy/            systemd units, OpenObserve settings, Tailscale policy
    analytics/         DuckDB queries over runs, traces and eval results

## Test

    ./scripts/coverage.sh                     # all tests + 100% line coverage
    ./scripts/e2e.sh                          # live, against OpenRouter

[TESTING.md](TESTING.md) covers the three test layers, the rules for schema
changes, and how an AI agent runs and extends the end-to-end test.

## Evaluation

Tests prove the code does what it says. Evals check that the agent (model,
preamble and tools together) still behaves: calls the right tools, answers
correctly, refuses what it must refuse.

    athena eval run --target replay                    # PR gate: no network, $0
    athena eval run --target http://127.0.0.1:8080 --k 3 --out live.jsonl
    athena eval compare base.jsonl live.jsonl          # regression check
    athena eval record --case add_tool                 # re-record, real model

A case is a JSON file in `evals/cases/`: user turns, the expected tool
trajectory, checks on the final reply, and thresholds.

```json
{
  "eval_case_id": "add_tool",
  "kind": "trajectory",
  "tags": ["core", "tools"],
  "turns": ["Use the add tool to add 21 and 21, then tell me the result."],
  "expect": {
    "trajectory": {"mode": "exact", "tools": [{"tool": "add", "args_match": "21"}],
                   "forbid": [{"tool": "*", "args_match": "passwd"}]},
    "output": {"contains": ["42"], "not_contains": ["sorry"], "regex": "\\b42\\b"},
    "rubric": "States that 21 + 21 is 42."
  },
  "thresholds": {"pass_rate": 1.0, "max_model_calls": 3, "max_total_tokens": 5000, "gate": true},
  "cassette": "../cassettes/add_tool.json"
}
```

- `kind` is `single`, `multi`, `trajectory`, `safety` or `regression`.
  Safety cases are scored pass^k (every one of `--k` samples must pass);
  the rest by `thresholds.pass_rate` (default 1).
- `expect.trajectory.mode`: `ordered` (default; these calls in this order,
  others allowed between), `subset` (any order) or `exact` (these calls and
  nothing else). A tool is a name or `{"tool", "args_match"}`, a regex
  searched in the arguments as compact JSON; `"*"` is any tool. Any call
  matching a `forbid` pattern fails the sample.
- `expect.output` checks the final reply: `contains` and `not_contains`
  are case-insensitive.
- Every sample must also finish without an error, stay within its budgets,
  and (where the target reports it) never stop at the output-token limit.
- `expect.rubric` is read only by `--judge`, an LLM judge on OpenRouter
  (`ATHENA_JUDGE_MODEL`, else `AGENT_MODEL`; three votes, majority). It is
  advisory: `scores.judge` and `scores.judge_pass`, never `pass`.
- `cassette` is relative to the case file; the default is
  `../cassettes/<eval_case_id>.json`.

The full schema is in the docs of `src/eval/mod.rs`.

**Replay** runs every case through the production agent, its real tools and
a fresh SQLite database, with a model that serves the case's cassette: the
responses recorded from a real model. The trajectory is rebuilt from the
stored messages, as production stores them. If the agent now asks the model
something the recording never saw (a tool returns something else, a tool is
gone, a turn was added), the case fails with `trajectory drift`: re-record
it. Preamble edits and new tools do not invalidate cassettes; live runs
catch those. Replay gates on every case.

**A URL target** drives a running `athena serve` through the HTTP API as
user `eval` (`--user`): a new session per sample, then the session's
messages. A case with `"gate": false` is reported without failing the run.

**Recording** (`athena eval record [--case ID]`) calls the real model, so it
needs `OPENROUTER_API_KEY` and costs money. A cassette is written only when
the recording passes the case's graders; a failing recording leaves the old
cassette alone. The four starter cassettes were scripted by hand from the
exchanges the tests use (their `model` is `scripted/starter`); record them
against your model before relying on them.

`--out` writes one JSON line per sample: `eval_run_id`, `git_sha`
(`ATHENA_VERSION`, else `GITHUB_SHA`, else `dev`), `env` (`ATHENA_ENV`),
`target`, `case_id`, `kind`, `tags`, `sample`, `pass`, `scores`, `reason`,
`run_id`, `session_id`, `trace_id` (from a `traceparent` or `x-trace-id`
response header), `tokens`, `model_calls`, `latency_ms`, `model`. The run
prints a summary table and exits non-zero if a gating case missed its
threshold.

`athena eval compare BASE CANDIDATE` prints pass rates by case and by tag and
exits non-zero on any drop in a safety case (or a safety case missing from
the candidate), or a drop of more than 5 points in any other case or tag.

## Load check

    athena bench --url http://127.0.0.1:8080 [--concurrency 3] [--duration-secs 120] \
                 [--user bench] [--max-p95-ms 30000] [--max-error-rate 0.05]

Each worker creates a session and sends short prompts, one turn at a time,
starting a new session every five turns, until the duration is up. These are
real turns on the deployed model and cost money. It prints a JSON summary:

```json
{"url": "...", "concurrency": 3, "duration_secs": 120, "requests": 96, "errors": 0,
 "error_rate": 0.0, "turns": 78, "latency_ms": {"p50": 2100, "p95": 5400, "max": 7900},
 "tokens": {"input": 9100, "output": 800, "total": 9900}, "max_p95_ms": 30000,
 "max_error_rate": 0.05, "pass": true, "failures": [], "first_error": null}
```

`requests` counts session creations too; latencies are successful turns
only. It exits non-zero when the p95 or the error rate is over its limit, or
when no turn succeeded.

## The core service

`service::Service` is the one API a transport calls. It returns values and
never prints; problems that do not fail a call (a lost telemetry row) go to
the `warn` callback given to `Service::new`.

    let service = Service::new(Store::open(path)?, model, warn);
    let agent   = agent::build(&client, model, service.memory());

    let user    = service.user("telegram", chat_id).await?;         // get or create
    let session = service.open_session(&user, "default").await?;    // get or create
    let session = service.create_session(&user, "notes").await?;    // AlreadyExists if taken
    let session = service.session(&user, &id).await?;               // NotFound unless theirs
    let list    = service.sessions(&user).await?;                   // name, id, message count
    let msgs    = service.history(&user, &id).await?;               // Vec<Message>
    let totals  = service.usage(&user).await?;
    let turn    = service.send(&agent, &user, &id, "hello").await?; // Turn { reply, run }

Errors are `service::Error`: `NotFound`, `AlreadyExists`, `Invalid`,
`Conflict`, `Model`, `Storage`, chosen to map straight onto HTTP statuses.

`send` guarantees:

- Turns on one session run one at a time. A second message that arrives
  while a turn is running waits, then loads the history the first one saved.
  Turns on different sessions run concurrently.
- The transcript is saved before `send` returns the reply. If it cannot be,
  `send` fails, even though the model answered; the run is still recorded
  with the tokens it spent.
- If another process appended to the session during the turn, the turn is
  refused with `Conflict` instead of interleaving two conversations that
  never saw each other.

The agent is a parameter of `send`, not part of the service, so listing and
creating sessions needs no API key and one service can serve any number of
tasks. Dropping `send`'s future cancels the turn. A transport whose client
can go away uses one of these instead (the service behind an `Arc`):

    let turn = service.send_detached(agent, &user, &id, "hi").await?;  // Turn
    let mut turn = service.send_stream(agent, &user, &id, "hi").await?;
    while let Some(event) = turn.next().await { ... }  // Text, ToolCall, then Done(Result<Turn>)
    service.idle().await;                             // on shutdown

Both check the request and wait for the session in the caller's future, so
a caller that gives up while queued behind another turn leaves nothing
behind. Once the turn holds the session it runs in its own task, with every
guarantee above, and finishes and is saved even if nobody is listening any
more. `idle` waits for those tasks. `send_stream` drives Rig's streaming
agent, which loads and appends through the same memory and receipt as the
blocking one; its `Done` carries exactly what `send` would have returned.

## HTTP API

`athena serve` listens on `--addr`, else `ATHENA_ADDR`, else
`127.0.0.1:8080`. It needs `OPENROUTER_API_KEY` at startup. It is
unauthenticated: read Known limits before listening anywhere but loopback.
On any other address it still starts, and prints a warning.

Every request except `/health` and `/version` names its user in a header:

    X-Athena-User: alice          # the user ("http", "alice")

If `alice` was linked with Telegram's `/link alice`, this identity resolves to
the Telegram user's existing `users.id`, including access to their sessions.

A missing, blank, non-ASCII or over-256-byte value is `400`. `src/http.rs`
turns the header into a user in one place, the `Caller` extractor, so
authentication replaces that and nothing else. While listening on loopback,
a request whose `Host` is not `localhost`, `127.0.0.1` or `[::1]` is `403`,
so a web page cannot reach the API by pointing its own domain at 127.0.0.1
(DNS rebinding).

`ATHENA_ALLOWED_HOSTS` (for example `100.64.0.1:18080,athena-vm:18080`) sets
the `Host` values the API answers wherever it listens: a `HOST` entry on any
port, a `HOST:PORT` entry on that port only, compared without case. On
loopback the loopback names are answered too. Any other `Host` is `403`, and
the UNAUTHENTICATED warning is not printed. This gives a server on a private
network address (a tailnet IP, say) the same DNS-rebinding protection as
loopback. It is not authentication: anyone who can reach the port and send a
listed `Host` is still trusted. Without it, a server on any other address
answers every `Host` and prints the warning.

| Method and path | Body | Success |
|---|---|---|
| `GET /health` | | `200 {"status":"ok"}` |
| `GET /version` | | `200 {"version":"..."}`, as `athena --version` prints it |
| `POST /sessions` | `{"name":"notes"}` | `201 {"id","name","created_at"}` |
| `GET /sessions` | | `200 {"sessions":[{"id","name","created_at","messages"}]}` |
| `GET /sessions/{id}/messages` | | `200 {"messages":[...]}` |
| `POST /sessions/{id}/messages` | `{"text":"hi"}` | `200 {"reply","run":{...}}` |
| `POST /sessions/{id}/messages/stream` | `{"text":"hi"}` | `200 text/event-stream` |
| `GET /usage` | | `200 {"usage":[{"session_id","name","runs","model_calls","input_tokens","output_tokens","cached_input_tokens"}]}`; a compaction's summary calls count as runs |

`messages` are Rig's own message JSON, exactly as stored, tool calls and
tool results included. That format belongs to Rig and can change with a Rig
upgrade.

`run` is the turn's `runs` row without `calls_json` (raw provider
responses): `run_id`, `session_id`, `status`, `error`, `model`,
`first_seq`, `last_seq`, `model_calls`, `input_tokens`, `output_tokens`,
`total_tokens`, `cached_input_tokens`, `reasoning_tokens`, `started_at`,
`ended_at`.

A streamed turn sends these events, then closes the stream:

    event: delta       data: {"text":"4"}                          0 or more
    event: tool_call   data: {"name":"add","arguments":{"a":21,"b":21}}  0 or more
    event: done        data: {"reply":"42","run":{...}}            last, on success
    event: error       data: {"error":{"code":"model","message":"..."}}  last, on failure

`done.reply` is the reply. Concatenated deltas can differ from it, because
text the model writes next to a tool call is streamed too. Keep-alive
comments (`:` lines) arrive every 15 s during long tool calls. A bad request
(bad body, empty text, someone else's session) is refused with a status
code before the stream starts.

Errors are `{"error":{"code","message"}}`:

| Status | `code` | When |
|---|---|---|
| 400 | `invalid` | bad or missing header, body not JSON, empty name or text |
| 403 | `forbidden_host` | see above |
| 404 | `not_found` | no such session **for this user**, including another user's |
| 409 | `already_exists` | session name taken |
| 409 | `conflict` | another process wrote to the session mid-turn; send again |
| 502 | `model` | the model failed; details in the server log and `runs.error` |
| 500 | `storage` | the database failed; details in the server log |
| (SSE) | `internal` | the turn crashed; details in the server log |

Another user's session id gets the same `404` as an id that does not exist.
`model` and `storage` messages are generic, because provider and database
errors can carry details an unauthenticated client should not see.

A turn that has started finishes and is saved even if its client
disconnects, streamed or not. Ctrl-C stops accepting requests, lets open
ones finish and waits for every turn still running, then exits. A second
Ctrl-C quits at once, and the turns in flight are lost as in a crash.

    curl -s localhost:8080/sessions -H 'X-Athena-User: alice' -d '{"name":"notes"}'
    curl -N localhost:8080/sessions/$ID/messages/stream -H 'X-Athena-User: alice' -d '{"text":"hi"}'

## How persistence works

Rig loads and saves the conversation itself, through
`store::SqliteMemory`, an implementation of Rig's `ConversationMemory` over
the `messages` table. The conversation id is the session id. Before a turn,
Rig loads the session's rows; after a successful turn it appends everything
the turn produced (the user message, each tool call and result, the reply) in
one call. The memory inserts those rows after the last one in a single
transaction and never rewrites an earlier row. Each row is opaque Rig JSON:
Rig's `Message` already encodes tool calls and results, and keeping it
opaque means a Rig upgrade does not need a schema change.

Rig treats a failed append as a warning and returns the reply anyway. The
memory records what each load and append did in a per-session receipt, and
`send` reads it after the run, which is how an unsaved transcript becomes an
error instead of a log line.

The database handle is one SQLite connection per process behind a mutex
(`store::Store`). SQL runs on tokio's blocking pool, never on an async worker,
and the mutex is never held across an `.await`, so a model call on one
session never holds up another. SQLite allows one writer at a time anyway,
so a pool or a dedicated writer task would add code without adding write
throughput. All SQL lives in `store.rs`, so changing the handle later
touches one file.

The schema is versioned with `PRAGMA user_version`. `Store::open` applies any
missing migrations in one transaction, refuses a database written by a newer
build, refuses a table whose columns it does not expect, and refuses one with
dangling references. Existing `agent.db` files upgrade in place: every
session they hold is given to `cli:local` under its old name, and no stored
message changes. The rules for adding a migration are in TESTING.md.

Foreign keys are enforced per connection. `Store` turns them on; a plain
`sqlite3` shell does not.

Inspect a session:

    sqlite3 agent.db "SELECT m.seq, json_extract(m.json,'\$.role'), substr(m.json,1,200)
                      FROM messages m JOIN sessions s ON s.id = m.session_id
                      WHERE s.name='default' ORDER BY m.seq;"

## Compaction

A session's transcript only grows, and Rig sends all of it to the model on
every call. When a session nears the model's context window, Athena replaces
the oldest part of what it sends with a summary. The transcript itself is
never rewritten: a compaction adds a row to the `compactions` table (session,
`through_seq`, summary, which model wrote it and what it cost), meaning "this
summary stands in for every message up to `through_seq`". The next turn loads
the newest summary followed by the rows after it. History, `sessions` message
counts and any search still see every original row. To undo a compaction,
delete its row (`DELETE FROM compactions WHERE session_id = ...`): the next
turn loads the whole transcript again.

When: before every model call, not once per turn, because one long tool loop
can outgrow the window inside a single turn. The size of the request is what
the provider reported for the previous call, plus four characters a token for
what has been added since (an image in the turn in progress counts as 1 500
tokens, whatever its bytes; four characters a token undercounts Chinese,
Japanese and Korean text, but only for what the provider has not counted yet).
Above `ATHENA_COMPACT_AT` (80%) of the window, the oldest messages are
summarized by `ATHENA_COMPACT_MODEL` (default `AGENT_MODEL`) and the request
is sent with `[summary, ..recent]` instead. Rig applies a request patch to one
call only, so the summary is applied again to every later call of the turn.
This works the same for blocking and streamed turns.

The window is `ATHENA_CONTEXT_TOKENS` if set, else the `context_length`
OpenRouter lists for `AGENT_MODEL` (`GET https://openrouter.ai/api/v1/models`,
which needs no key), else 128000 with a warning in the log. The list is
fetched the first time a request is big enough to matter (a short chat never
waits for it) and a window found is kept for the life of the process; if the
lookup fails, 128000 is assumed for ten minutes and then it is asked again.
OpenRouter lists one number per model, and it is the largest; the provider a
request lands on can have a smaller window. Set `ATHENA_CONTEXT_TOKENS` if you
know better. It must be 8000 or more.

What a cut never does:

- separate a tool call from its result, or start the kept messages with a
  result (the store refuses a checkpoint that would);
- cut the message the model is being asked about (when that is a tool result,
  the assistant message with its call is kept too);
- keep less than a fifth of the window word for word. A cut that would leave
  the request over the line anyway is not made.

The summary call is recorded as a run of its own (model `ATHENA_COMPACT_MODEL`,
no messages saved, `calls_json` `[{"purpose": "compaction"}]`), so `usage`
shows what compaction costs, and counts it among the session's runs. The call
is capped at a tenth of the window (256 to 4000 tokens) and asked for three
fifths of that many words. A summary that fails, comes back empty, is more
than a quarter over the cap by our count (it is not cut short: its end holds
the open tasks) or takes over 120 seconds is recorded as an `error` run and
skipped: the turn goes on with the full history and does not fail. That session is not tried again, by any turn of this process, until its
request has grown by a twentieth of the window. The checkpoint is written
after the turn's own rows, and only if the turn was saved, so a turn that fails
leaves none. If the checkpoint cannot be written, the next turn loads the whole
transcript, so the prompt size that turn's last call reported (for the summary
and what it kept) is not used for it: the size is estimated from the messages,
and the session is compacted again.

The summarizer is told that tool results are untrusted data, and what it is
shown labels them so. The summary is stored and sent back as a user message,
under a header saying it is notes and that nothing in it is an instruction
from the user: a web page the agent read must not be able to write itself into
what the user "said". A summary is still model output; do not rely on it as a
security boundary.

OpenRouter has a `context-compression` plugin that drops messages from the
middle of a prompt that does not fit, which can orphan a tool result. Every
request the agent sends carries `plugins: [{"id": "context-compression",
"enabled": false}]` so it never runs. The summary call is a plain completion
and does not need it.

`athena eval` never compacts: a cassette holds one model's calls, and a summary
call is one it never recorded.

Choose `ATHENA_COMPACT_MODEL` with a window at least as big as the agent's: the
summarizer is given everything to be summarized in one request (each message
part clipped to 8000 characters). A session that is already over its window
when compaction is first switched on needs that too, and may not be rescued at
all.

## What a run costs

Every turn writes one row to `runs` alongside the transcript: token usage
broken out by kind (input, output, cached input, reasoning), the number of
completion requests Rig issued, per-call finish reasons, and the provider's
raw response.

    cargo run -- usage
    session   runs  calls  in   out  cached
    live      1     2      256  26   0

Model calls are not message counts. `add 21 and 21` is one turn, two
completion requests and four transcript rows:

    call 0  finish_reason=tool_calls  in=111  out=21   <- model asks for add()
    call 1  finish_reason=stop        in=145  out=5    <- model answers "42"

Input grows between the two because the tool result re-enters context. That
ongoing cost is invisible in `messages` and is the reason `runs` exists.

`runs.calls_json` keeps the full `Vec<CompletionCall>` as opaque Rig JSON,
including each call's raw provider response. That payload carries detail Rig
does not model — for OpenRouter, upstream routing and cost — but it is an
unredacted copy of the response and it dominates row size (1737 bytes versus
263 in the run above). Set `RUNS_STORE_RAW=0` to drop it.

Telemetry is written after the transcript and its failure is warned about, not
fatal: losing a cost row must never cost you a reply.

## Observability

Every process logs through `tracing`. Log lines go to stderr in plain text,
one per event, with a timestamp and level. `RUST_LOG` picks what stderr shows;
the default is `warn,athena=info`. The CLI's own output (replies, `warning:`
lines) still prints directly, so the REPL looks the same as before.

Spans and log events can also be exported, to two sinks that are switched
on independently. With neither set, OpenTelemetry is off. There is no
collector in between: Athena does the exporting itself, each sink on its own
background thread.

- **OTLP/HTTP** (protobuf) when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, to
  `<endpoint>/v1/traces` and `<endpoint>/v1/logs`. On the VM that is
  OpenObserve: `http://<tailnet-ip>:5080/api/default` with
  `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Basic%20<base64 of user:password>`
  (`%20` is the space; values are URL-decoded). The other standard
  `OTEL_EXPORTER_OTLP_*` variables (timeout, per-signal endpoints) work too.
  Staging and prod both write to OpenObserve's `default` organization, so
  you see one space. Telling them apart is a filter, not a separate place:
  every record carries `deployment.environment.name` (`ATHENA_ENV`). In
  OpenObserve that is the field `service_deployment_environment_name` on traces
  and `deployment_environment_name` on logs, for example
  `service_deployment_environment_name = 'staging'` in the traces search. If a
  query finds nothing, check the stream's schema in the UI for the exact name.
- **JSON Lines files** when `ATHENA_TELEMETRY_DIR` is set:
  `traces-<role>-YYYYMMDD.jsonl` and `logs-<role>-YYYYMMDD.jsonl`, where the
  role is the process (`serve`, `telegram` or `cli`) so each file has exactly
  one writer. One object per line, a new file each UTC day,
  files older than `ATHENA_TELEMETRY_RETENTION_DAYS` deleted. The schema is
  Athena's own and flat (`trace_id`, `span_id`, `parent_span_id`, `name`,
  `start_unix_nano`, `duration_ms`, `status`, `attributes`, `resource`, ...);
  `src/telemetry/jsonl.rs` documents every field. `scripts/analytics.sh`
  queries them with DuckDB.

Buffered data is exported on exit, after `serve` or `telegram` has let its
turns finish. There is no retry queue in front of OpenObserve: batches sent
while it is down or restarting are lost there, but the JSONL files still
have them. Nor is there the collector's old regex masking of secrets in
attribute values; Athena does not put keys or tokens on spans or in log
fields, and nothing now checks that for it.

| Variable | Default | Effect |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset (no OTLP) | OTLP/HTTP base URL, e.g. `http://100.x.y.z:5080/api/default` |
| `OTEL_EXPORTER_OTLP_HEADERS` | unset | request headers, `key=value,...`, e.g. OpenObserve's basic auth |
| `ATHENA_TELEMETRY_DIR` | unset (no files) | directory for the daily JSONL files, e.g. `/var/lib/athena/prod/telemetry` |
| `ATHENA_TELEMETRY_RETENTION_DAYS` | `30` | days of files kept, today included |
| `OTEL_SERVICE_NAME` | `athena` | resource `service.name` |
| `ATHENA_VERSION` | `<crate version>-dev` | resource `service.version` |
| `ATHENA_ENV` | unset | resource `deployment.environment.name` |
| `RUST_LOG` | `warn,athena=info` | stderr filter only; export always takes `info` and up |

What a turn exports:

- `invoke_agent athena`, one span per turn, from `Service`. It carries
  `gen_ai.operation.name`, `gen_ai.agent.name`, `gen_ai.conversation.id` (the
  session id), `athena.run_id` (join it to the `runs` table),
  `athena.transport`, `enduser.pseudo.id`, and the run's input and output
  token totals. A failed turn has status `ERROR` and `error.type` (`model`,
  `storage`, `conflict`).
- Rig's `chat` or `chat_streaming` span per model call and `execute_tool` per
  tool call, nested under it. Rig adopts Athena's span rather than opening
  its own.
- For HTTP, a server span per request named after the route, such as
  `POST /sessions/{id}/messages`. A W3C `traceparent` header on the request
  makes it part of the caller's trace. Responses carry `x-trace-id` and
  `traceparent` so an eval or client can look the trace up. Without
  OpenTelemetry neither header is sent.

`enduser.pseudo.id` is the first 16 bytes of SHA-256 over
`transport:external_id`, in hex. It keeps raw Telegram ids out of the
backend, but it is unsalted, and Telegram ids are numbers anyone can
enumerate. Treat it as internal data, not as anonymous.

Everything is exported, always: there is no switch. In every environment,
prod included, Rig records the prompt on the turn span and model input, output, the system prompt, tool arguments and tool
results on its own spans (`gen_ai.prompt`, `gen_ai.input.messages`,
`gen_ai.output.messages`, `gen_ai.system_instructions`,
`gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`), and both sinks
export them as they are: OpenObserve and the JSONL files, kept for
`ATHENA_TELEMETRY_RETENTION_DAYS` (30). That is whatever users typed, so treat
both as holding user data. To keep content out, remove the sinks. Athena's own
log events never include prompt or reply text.

## Known limits

- **The HTTP API is unauthenticated. Do not expose it publicly.** Whoever
  can reach the port can act as any user by setting `X-Athena-User`, read
  every session, spend the OpenRouter credit, and use the agent's tools in
  any session's sandbox. Loopback is the default, and a loopback server
  refuses foreign `Host` headers, but every local process is still trusted. Authentication
  replaces `Caller` in `src/http.rs`; until then, put an authenticating
  proxy in front before listening anywhere else.
- Every distinct `X-Athena-User` value creates a `users` row, even for a
  read. There is no rate limit or cap beyond the 256-byte header limit.
- A crash mid-turn loses that turn, and so does a second Ctrl-C while
  `serve` drains. Persistence granularity is one append per turn, not per
  model call.
- Dropping `send`'s future cancels the turn. If that happens after the
  append but before the run row, the transcript has rows no run covers.
  `send_detached` and `send_stream` do not have this problem.
- A turn that panics (a bug) records no run. The session is unlocked, and
  a streamed client gets an `internal` error event.
- A streamed turn's `calls_json` holds what Rig's streaming driver
  reports for each call. With Rig's mock model that raw payload is the
  stream's terminal record, not a whole response; it has not yet been
  compared with a blocking call's payload from OpenRouter. The blocking
  HTTP endpoint and the CLI use the blocking driver.
- Deltas already streamed cannot be taken back. If the transcript then
  fails to save, the client has seen text that the `error` event says was
  not kept.
- Axum answers some malformed requests itself, not in this API's JSON
  error shape: unknown paths (empty `404`), wrong methods (`405`), and
  bodies over 2 MB (`413`).
- Compaction is a summary, so it is lossy, and it waits until the context is
  at 80% of the model's window: with the default model that is 840 000 tokens
  re-sent on every call until it happens. It cuts only between messages, so
  one prompt or tool result that alone fills the window cannot be helped. A
  session already over its window when compaction is switched on may not be
  rescued, and a summary model with a smaller window than the agent's fails
  on a big session (the turn goes on without compacting). The window is
  OpenRouter's largest for the model, not the provider's. There is no
  `/compact` command and no token cap below the percentage yet. See Compaction.
- Transports so far: the CLI, HTTP and Telegram. They call
  `service::Service`.
- The Telegram bot is open to anyone. Whoever finds its username can talk
  to it, and every turn they run spends the owner's OpenRouter credit. There
  is no allowlist and no per-user budget; `/usage` and the `runs` table show
  who spent what, by Telegram user id.
- Telegram delivery is at most once. An update counts as received when the
  next poll starts, not when it is answered, so a crash or `kill -9` loses
  messages that were queued, and a reply whose send fails is lost even
  though its transcript is saved.
- Stopping takes as long as the slowest turn in flight, and for the bot up
  to one more long poll (10 s). A second signal quits `serve` at once; the
  bot ignores it, and ignores a signal that arrives before it starts
  polling. Docker waits 10 s and systemd 90 s before
  SIGKILL, which cannot be caught and loses those turns: raise the grace
  period (`docker stop -t`, `stop_grace_period`, `TimeoutStopSec`) to cover
  a slow tool-using turn.
- Telegram messages have no headings, lists or tables, so Markdown is
  approximated, not reproduced: a heading is bold text, a list is `•` or
  numbered lines, a table is monospace (and one wider than 60 columns is a
  `Header: value` block per row, since a grid that wide does not fit a
  phone). Telegram does not let `code`, links or blockquotes contain other
  formatting, so bold code is code only, and code or a table inside a quote
  loses its monospace styling. A link inside a quote or a table cell, and a
  link whose address is not http, https or mailto (`#section`, a relative
  path), is shown as `text (address)` and is not tappable. Telegram's
  documented nesting rules are followed to the letter; clients may accept
  more, but that is not checked here. Raw HTML is shown as written. Table widths use an
  approximation of Unicode's East Asian Width, so some emoji and rare
  scripts can misalign a grid.
- Edited messages, stickers, video notes and other messages that are neither
  text, a photo, a file nor a voice note are not prompts. Edits are ignored;
  the rest get "I read text, voice notes, photos and files, not this kind of
  message." Without the Cloudflare settings, voice notes and audio files are
  not prompts either, and the reply leaves out voice notes.
- Voice notes are transcribed by Cloudflare, so their audio leaves the
  machine, and every user who can reach the bot spends the owner's Workers
  AI credit; there is no per-user cap. Each audio file of an album is its own
  turn, so the second gets the busy reply. A transcription may take up to
  90 seconds before the turn starts, which counts toward the stop timeout.
- An album is collected until 2 seconds pass with no new item from it, so
  its reply starts 2 seconds late. An item that Telegram delivers later than
  that becomes a turn of its own, or gets the busy reply if the album's turn
  is still running. At most 4 photos are shown to the model at once; the
  rest of an album are in the sandbox and the model opens them with
  `view_image`.
- Whether the model can see images depends on `AGENT_MODEL`. The default,
  `openai/gpt-6-luna`, accepts image input on OpenRouter; with a model that
  does not, the provider may refuse turns that carry a photo or a screenshot.
- The selected session is Telegram state. The CLI still uses `default`
  unless given a session name, and `sessions` does not mark the selection
  (`selected_sessions` is readable with `sqlite3`). `Service` does not
  expose selection yet; `telegram.rs` reads it from `Store` directly.
- Provider errors are logged in full (stderr, and OTLP when it is on) and
  can include account details from the provider's error body. Message text
  is never logged.
- Turns on one session are serialized within a process. Across processes
  they are not: the second one to finish is refused with `Conflict` rather
  than interleaved.
- No timing beyond whole-turn wall clock. Rig reports no per-call latency;
  time-to-first-token and per-tool duration need Rig's hooks.
- `provider_request_id` is often empty — OpenRouter does not always report one.
- **Sign-in links are not secured yet.** The token in the link is the only
  check: anyone who can reach `athena serve` with the link can drive that
  browser, signed in as the user, for an hour. There is no Tailscale
  identity check and links cannot be revoked. Spans name the route, not the
  token.
- Saved sign-ins are stored unencrypted in `browser_states` and loaded into
  every sandbox of their user, where `shell`, `read_file` and
  `agent_browser` can read or export them. A prompt injection can therefore
  send a user's cookies anywhere. The state is one per user for every site,
  and the last Done wins; a new link loads the latest state first, so
  sign-ins made in other conversations are kept.
- Text typed on the sign-in page reaches the sandbox as a shell command
  line (`agent-browser keyboard type '<text>'`) through the OpenSandbox
  proxy, unencrypted on the tailnet. It is not logged or put on spans.
- The sign-in page and the agent can drive one browser at the same time:
  `serve` and `telegram` are separate processes, and turns are serialised
  only within one. Each page poll renews the sandbox and writes its row.
- Headless Chromium on a server is refused by some sign-ins (Google's among
  them) and challenged by others; the page cannot get past a CAPTCHA the
  site shows.
- A stdio MCP server listed in `ATHENA_MCP_CONFIG` runs on the host with
  athena's permissions, outside the sandbox, and one connection serves every
  user. Anything that can write that file or its environment variables
  chooses what runs. Tokens passed as `args` show in `ps`; put them in `env`.
  A server that dies after startup stays dead until athena restarts, and a
  name clash costs the later tool (see "Tools from MCP servers").
- `runs` rows are ordered by `(started_at, rowid)`. `VACUUM` may renumber
  rowids, so two runs started in the same millisecond can swap after one.
