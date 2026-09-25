use opencrab_web_gateway::{
    owner::{run, OwnerConfig, MASTER_KEY_ENV},
    store::WebStore,
};
use std::time::Duration;

#[tokio::test]
async fn d_1006_web_01_existing_instance_starts_without_bearer_or_web_master_key() {
    let temp = tempfile::tempdir().unwrap();
    let database_path = temp.path().join("web.db");
    let admin_socket = temp.path().join("web-admin.sock");
    let store = WebStore::open(&database_path).unwrap();
    store
        .configure("127.0.0.1:0", "/tmp/stopped-core.sock")
        .unwrap();
    store
        .upsert(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent-web",
            1,
            "web-author",
            None,
            true,
            &[7; 32],
        )
        .unwrap();
    drop(store);
    rusqlite::Connection::open(&database_path).unwrap().execute(
        "INSERT INTO identity_projections(instance_id,role,external_id) VALUES (?1,'owner','web-local')",
        ["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"],
    ).unwrap();
    assert!(
        std::env::var(MASTER_KEY_ENV).is_err(),
        "fixture must have no Web master key"
    );
    let config = OwnerConfig {
        database_path,
        admin_socket: admin_socket.clone(),
        admin_instance_ids: vec!["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()],
    };
    let mut owner = tokio::spawn(run(config));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if admin_socket.exists() {
                break;
            }
            if owner.is_finished() {
                let error = (&mut owner)
                    .await
                    .unwrap()
                    .expect_err("Web owner stopped before serving");
                panic!("historical credential-free Web instance failed startup: {error:#}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Web owner did not start");
    assert!(!owner.is_finished());
    owner.abort();
    let _ = owner.await;
}
