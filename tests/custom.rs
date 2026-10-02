//! The owner's instructions and skills (`ATHENA_INSTRUCTIONS`,
//! `ATHENA_SKILLS_DIR`) through the real agent, built by
//! `agent::configure_custom` as production builds it, in front of a
//! scripted model; and through the built binary at startup.

mod common;

use athena::agent;
use athena::custom::{Config, Custom};
use athena::service::Service;
use common::*;
use rig_agent::agent::AgentBuilder;
use rig_agent::prelude::Message;
use rig_core::completion::CompletionRequest;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;
use std::io::{BufRead, Read};

/// A directory with the files an owner would write.
struct Home {
    custom: Custom,
    warnings: Vec<String>,
}

fn write(dir: &WorkDir, name: &str, contents: &str) {
    let path = dir.path().join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn skill(dir: &WorkDir, name: &str, description: &str, body: &str) {
    write(
        dir,
        &format!("skills/{name}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    );
}

impl Home {
    fn new(setup: impl FnOnce(&WorkDir)) -> Self {
        let dir = WorkDir::new();
        setup(&dir);
        let config = Config {
            instructions: Some(dir.path().join("instructions.md")),
            skills_dir: Some(dir.path().join("skills")),
        };
        let (custom, warnings) = Custom::load(&config);
        Self { custom, warnings }
    }

    /// The production agent with this home's files, over the service's memory.
    fn agent(
        &self,
        service: &Service,
        turns: Vec<MockTurn>,
    ) -> (rig_agent::agent::Agent, MockCompletionModel) {
        let model = MockCompletionModel::new(turns);
        let builder = AgentBuilder::new(model.clone()).memory(service.memory());
        (agent::configure_custom(builder, None, &self.custom), model)
    }
}

fn system_prompt(request: &CompletionRequest) -> &str {
    match &request.chat_history[0] {
        Message::System { content } => content,
        other => panic!("not a system message: {other:?}"),
    }
}

fn tool_names(request: &CompletionRequest) -> Vec<&str> {
    let mut names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    names.sort();
    names
}

/// The text of the tool result the model was sent last.
fn tool_result(request: &CompletionRequest) -> String {
    let last = serde_json::to_value(request.chat_history.last().unwrap()).unwrap();
    let content = &last["content"][0];
    assert_eq!(content["type"], "toolresult", "{last}");
    content["content"][0]["text"].as_str().unwrap().to_string()
}

fn read_skill_turns(name: &str) -> Vec<MockTurn> {
    vec![
        MockTurn::tool_call("call_1", "read_skill", json!({ "name": name })),
        MockTurn::text("done"),
    ]
}

/// Run one turn of `turns` and return the two requests the model got.
async fn run(home: &Home, turns: Vec<MockTurn>) -> Vec<CompletionRequest> {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let (agent, model) = home.agent(&service, turns);
    service.send(&agent, &user, &s.id, "go").await.unwrap();
    model.requests()
}

#[tokio::test]
async fn the_model_is_sent_the_instructions_and_the_skills_after_the_base_preamble() {
    let home = Home::new(|dir| {
        write(dir, "instructions.md", "Call the user Boss.\n");
        skill(dir, "pdf-tools", "Work with PDFs.", "Open it.");
        skill(dir, "data-analysis", "Analyse data.", "Plot it.");
    });
    assert!(home.warnings.is_empty(), "{:?}", home.warnings);

    let requests = run(&home, vec![MockTurn::text("hi")]).await;
    let prompt = system_prompt(&requests[0]);
    assert!(prompt.starts_with(agent::PREAMBLE));
    let rest = &prompt[agent::PREAMBLE.len()..];
    assert!(
        rest.starts_with("\n\n## Custom instructions\n\nCall the user Boss.\n\n## Skills\n"),
        "{rest}"
    );
    assert!(rest.ends_with("\n\n- data-analysis: Analyse data.\n- pdf-tools: Work with PDFs."));
    assert_eq!(tool_names(&requests[0]), ["add", "read_skill"]);
}

#[tokio::test]
async fn without_any_files_the_model_sees_exactly_the_base_agent() {
    let home = Home::new(|_| {});
    assert_eq!(home.warnings.len(), 2, "{:?}", home.warnings);

    let requests = run(&home, vec![MockTurn::text("hi")]).await;
    assert_eq!(system_prompt(&requests[0]), agent::PREAMBLE);
    assert_eq!(tool_names(&requests[0]), ["add"]);
}

#[tokio::test]
async fn read_skill_returns_the_skills_instructions_to_the_model() {
    let home = Home::new(|dir| {
        skill(
            dir,
            "pdf-tools",
            "Work with PDFs.",
            "# Steps\n\n1. Open it.",
        );
    });
    let requests = run(&home, read_skill_turns("pdf-tools")).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(tool_result(&requests[1]), "# Steps\n\n1. Open it.");
}

#[tokio::test]
async fn an_unknown_skill_is_an_error_the_model_can_read() {
    let home = Home::new(|dir| skill(dir, "pdf-tools", "Work with PDFs.", "Open it."));
    let requests = run(&home, read_skill_turns("nope")).await;
    let result = tool_result(&requests[1]);
    assert!(result.contains("no skill named `nope`"), "{result}");
    assert!(result.contains("pdf-tools"), "{result}");
}

/// The skill `evals/live/skill_read_on_demand.json` depends on loads
/// cleanly, and the case names it and parses.
#[test]
fn the_live_eval_fixture_loads_and_its_case_names_the_skill() {
    let root = env!("CARGO_MANIFEST_DIR");
    let config = Config {
        instructions: None,
        skills_dir: Some(format!("{root}/evals/fixtures/skills").into()),
    };
    let (custom, warnings) = Custom::load(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    let preamble = custom.preamble("");
    assert!(
        preamble.contains("- house-style: How release notes"),
        "{preamble}"
    );

    let cases =
        athena::eval::case::load(std::path::Path::new(&format!("{root}/evals/live"))).unwrap();
    let case = cases
        .iter()
        .find(|c| c.eval_case_id == "skill_read_on_demand")
        .unwrap();
    let tool = &case.expect.trajectory.as_ref().unwrap().tools[0];
    assert_eq!(tool.tool(), "read_skill");
    assert_eq!(tool.args_match(), Some("house-style"));
}

#[tokio::test]
async fn a_name_that_climbs_out_of_the_skills_directory_reads_nothing() {
    let home = Home::new(|dir| {
        skill(dir, "pdf-tools", "Work with PDFs.", "Open it.");
        write(
            dir,
            "secret/SKILL.md",
            "---\nname: secret\ndescription: d\n---\nSECRET",
        );
        write(dir, "notes.txt", "SECRET");
    });
    for attempt in ["../secret", "../notes.txt", "pdf-tools/../../secret"] {
        let requests = run(&home, read_skill_turns(attempt)).await;
        let result = tool_result(&requests[1]);
        assert!(result.contains("no skill named"), "{attempt}: {result}");
        assert!(!result.contains("SECRET"), "{attempt}");
    }
}

#[tokio::test]
async fn a_skill_that_links_outside_the_directory_is_not_offered_or_readable() {
    let home = Home::new(|dir| {
        skill(dir, "pdf-tools", "Work with PDFs.", "Open it.");
        write(
            dir,
            "elsewhere/SKILL.md",
            "---\nname: linked\ndescription: d\n---\nSECRET",
        );
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere"),
            dir.path().join("skills/linked"),
        )
        .unwrap();
    });
    assert_eq!(home.warnings.len(), 2, "{:?}", home.warnings);
    assert!(
        home.warnings[1].contains("`linked` skipped"),
        "{:?}",
        home.warnings
    );

    let requests = run(&home, read_skill_turns("linked")).await;
    assert!(!system_prompt(&requests[0]).contains("linked"));
    let result = tool_result(&requests[1]);
    assert!(!result.contains("SECRET"), "{result}");
}

#[tokio::test]
async fn one_invalid_skill_does_not_stop_the_others() {
    let home = Home::new(|dir| {
        skill(dir, "good", "Fine.", "ok");
        write(
            dir,
            "skills/Bad-Name/SKILL.md",
            "---\nname: Bad-Name\ndescription: d\n---\n",
        );
    });
    assert!(
        home.warnings
            .iter()
            .any(|w| w.contains("`Bad-Name` skipped"))
    );
    let requests = run(&home, read_skill_turns("good")).await;
    assert_eq!(tool_result(&requests[1]), "ok");
}

// ---- the real binary ----

/// `athena serve` with these files configured logs a warning for each
/// problem, still starts, and serves.
#[test]
fn the_binary_warns_about_bad_files_and_still_starts() {
    let dir = WorkDir::new();
    let tmp = TempDb::new();
    skill(&dir, "good", "Fine.", "ok");
    write(
        &dir,
        "skills/Bad-Name/SKILL.md",
        "---\nname: Bad-Name\ndescription: d\n---\n",
    );
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_athena"))
        .args(["serve", "--addr", "127.0.0.1:0"])
        .current_dir(dir.path())
        .env("ATHENA_DB", tmp.path())
        .env("OPENROUTER_API_KEY", "unused-key")
        .env("ATHENA_INSTRUCTIONS", dir.path().join("missing.md"))
        .env("ATHENA_SKILLS_DIR", dir.path().join("skills"))
        .env_remove("OPEN_SANDBOX_URL")
        .env_remove("ATHENA_ADDR")
        .env_remove("ATHENA_ALLOWED_HOSTS")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("ATHENA_TELEMETRY_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut before_listening = String::new();
    loop {
        let mut line = String::new();
        let read = stderr.read_line(&mut line).unwrap();
        before_listening.push_str(&line);
        if read == 0 || line.starts_with("listening on http://") {
            break;
        }
    }
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}: {before_listening}{rest}");

    assert!(
        before_listening.contains("listening on http://"),
        "{before_listening}"
    );
    assert!(
        before_listening.contains("instructions ignored")
            && before_listening.contains("missing.md"),
        "{before_listening}"
    );
    assert!(
        before_listening.contains("`Bad-Name` skipped"),
        "{before_listening}"
    );
    assert!(!before_listening.contains("`good`"), "{before_listening}");
}
