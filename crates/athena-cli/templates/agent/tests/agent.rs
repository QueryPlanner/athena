//! The agent through the real runtime, against a scripted model: no API key,
//! no network.

use athena_core::service::Service;
use athena_core::store::Store;
use rig_agent::agent::AgentBuilder;
use rig_agent::prelude::Message;
use rig_core::test_utils::{MockCompletionModel, MockTurn};

#[tokio::test]
async fn a_turn_offers_this_agents_tools_and_uses_its_instructions() {
    let spec = @NAME@::agent::spec();
    let service = Service::new(spec.name, Store::open_in_memory().unwrap(), "mock", |_| {});
    let model = MockCompletionModel::new(vec![MockTurn::text("hello")]);
    let agent = spec.configure(AgentBuilder::new(model.clone()).memory(service.memory()));
    let user = service.user("test", "alice").await.unwrap();
    let session = service.create_session(&user, "s").await.unwrap();

    let turn = service
        .send(&agent, &user, &session.id, "hi")
        .await
        .unwrap();

    assert_eq!(turn.reply, "hello");
    let request = &model.requests()[0];
    let tools: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tools, ["add"]);
    // Rig sends the instructions as the first System message of the history.
    let told = request
        .chat_history
        .iter()
        .any(|m| matches!(m, Message::System { content } if content == spec.preamble));
    assert!(told, "the model was not sent prompts/system.md");
}
