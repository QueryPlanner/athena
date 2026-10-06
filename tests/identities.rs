//! Identity claims across independent connections, as serve and Telegram use.

mod common;

use athena::service::Error;
use common::*;
use std::sync::Arc;
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_link_and_first_http_request_keep_one_binding_without_orphan_users() {
    let tmp = TempDb::new();
    let (telegram, _) = tmp.service();
    let owner = telegram.user("telegram", "42").await.unwrap();
    let private = telegram.open_session(&owner, "notes").await.unwrap();
    let (http, _) = tmp.service();
    let mut expected_users = 2;
    for n in 0..16 {
        let name = format!("race-{n}");
        let barrier = Arc::new(Barrier::new(2));
        let first = barrier.clone();
        let (linked, caller) = tokio::join!(
            async {
                first.wait().await;
                telegram.link_http_user(&owner, &name).await
            },
            async {
                barrier.wait().await;
                http.user("http", &name).await
            }
        );
        let caller = caller.unwrap();
        match linked {
            Ok(()) => {
                assert_eq!(caller.id(), owner.id());
                assert_eq!(http.session(&caller, &private.id).await.unwrap(), private);
            }
            Err(Error::Invalid(_)) => {
                expected_users += 1;
                assert_ne!(caller.id(), owner.id());
                assert!(matches!(
                    http.session(&caller, &private.id).await,
                    Err(Error::NotFound)
                ));
            }
            Err(e) => panic!("unexpected link failure: {e}"),
        }
        assert_eq!(http.user("http", &name).await.unwrap().id(), caller.id());
        assert_eq!(
            count(&tmp.raw(), "SELECT COUNT(*) FROM users"),
            expected_users
        );
        assert_eq!(
            count(&tmp.raw(), "SELECT COUNT(*) FROM user_identities"),
            3 + n
        );
    }
    assert_eq!(
        count(&tmp.raw(), "SELECT COUNT(*) FROM pragma_foreign_key_check"),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_users_racing_to_link_an_identity_get_one_winner_and_keep_their_sessions() {
    let tmp = TempDb::new();
    let (first, _) = tmp.service();
    let (second, _) = tmp.service();
    let a = first.user("telegram", "1").await.unwrap();
    let b = second.user("telegram", "2").await.unwrap();
    let a_session = first.open_session(&a, "default").await.unwrap();
    let b_session = second.open_session(&b, "default").await.unwrap();
    let barrier = Barrier::new(2);
    let (a_link, b_link) = tokio::join!(
        async {
            barrier.wait().await;
            first.link_http_user(&a, "shared-api").await
        },
        async {
            barrier.wait().await;
            second.link_http_user(&b, "shared-api").await
        },
    );
    assert_ne!(a_link.is_ok(), b_link.is_ok());
    let (winner, loser, winner_session, loser_session, conflict) = if a_link.is_ok() {
        (&a, &b, &a_session, &b_session, b_link)
    } else {
        (&b, &a, &b_session, &a_session, a_link)
    };
    assert!(matches!(conflict, Err(Error::Invalid(_))));
    let caller = first.user("http", "shared-api").await.unwrap();
    assert_eq!(caller.id(), winner.id());
    assert_eq!(
        first.session(&caller, &winner_session.id).await.unwrap(),
        *winner_session
    );
    assert!(matches!(
        first.session(&caller, &loser_session.id).await,
        Err(Error::NotFound)
    ));
    assert_eq!(
        second.session(loser, &loser_session.id).await.unwrap(),
        *loser_session
    );
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM users"), 3);
}

#[tokio::test]
async fn a_storage_failure_is_reported_without_claiming_an_identity() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let owner = service.user("telegram", "42").await.unwrap();
    tmp.raw()
        .execute_batch(
            "CREATE TRIGGER refuse_link BEFORE INSERT ON user_identities
         BEGIN SELECT RAISE(ABORT, 'fixture failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        service.link_http_user(&owner, "new-api").await,
        Err(Error::Storage(_))
    ));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM user_identities WHERE transport = 'http'"
        ),
        0
    );
    // A trigger can also suppress an insert without returning an SQL error.
    // The service must verify a binding exists before claiming success.
    tmp.raw()
        .execute_batch(
            "DROP TRIGGER refuse_link;
         CREATE TRIGGER ignore_link BEFORE INSERT ON user_identities
         BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    assert!(matches!(
        service.link_http_user(&owner, "new-api").await,
        Err(Error::Storage(_))
    ));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM user_identities WHERE transport = 'http'"
        ),
        0
    );
}
