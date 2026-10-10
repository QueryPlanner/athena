//! The upload backstop of `health_export`: [`upload`] refuses a chunk that
//! would take the export past the cap, before anything is sent. The size
//! estimate normally stops an export first; this is the check on the data as
//! it is actually read.

use super::*;
use crate::sandbox::Config;
use crate::store::HealthWindow;
use std::time::Duration;

fn job() -> Job {
    let store = Store::open_in_memory().unwrap();
    let config = Config {
        url: url::Url::parse("http://127.0.0.1:9").unwrap(),
        api_key: None,
        image: "athena-sandbox:test".into(),
        timeout: Duration::from_secs(600),
        env: "test".into(),
        cpu: "500m".into(),
        memory: "1Gi".into(),
        startup_timeout: Duration::from_secs(5),
        startup_poll: Duration::from_millis(1),
        viewer_url: url::Url::parse("http://athena.test:18080").unwrap(),
    };
    let zone = TimeZone::UTC;
    Job {
        sandboxes: Arc::new(Sandboxes::new(config, store.clone())),
        store,
        session: "s".into(),
        owner: 1,
        window: HealthWindow::new(None, None, &zone).unwrap(),
        zone,
        types: Vec::new(),
        now: Timestamp::UNIX_EPOCH,
    }
}

#[tokio::test]
async fn a_chunk_past_the_cap_is_refused_and_kept_unsent() {
    let job = job();
    let mut buffer = vec![b'x'; 10];
    let sent = MAX_EXPORT_BYTES as usize - 5;
    let err = upload(&job, "/tmp/athena-data/.export-x", 1, &mut buffer, sent)
        .await
        .unwrap_err();
    assert!(
        err.contains("larger than the 128 MiB an export may hold"),
        "{err}"
    );
    assert_eq!(
        buffer.len(),
        10,
        "the buffer is not emptied when nothing is sent"
    );
}
