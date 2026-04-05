//! Integration tests — wiremock for forwardemail, real git store, no
//! in-process mocks.

use pimsteward::forwardemail::Client;
use pimsteward::pull::contacts::pull_contacts;
use pimsteward::pull::sieve::pull_sieve;
use pimsteward::store::Repo;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(base: &str) -> Client {
    Client::new(
        base.to_string(),
        "alice@test.example".to_string(),
        "pw".to_string(),
    )
    .unwrap()
}

#[tokio::test]
async fn contacts_pull_creates_then_updates_then_deletes() {
    let server = MockServer::start().await;
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(repo_dir.path()).unwrap();

    // --- First pull: two contacts, both new ---
    let contacts_v1 = serde_json::json!([
        {
            "id": "c1", "uid": "u1",
            "full_name": "Alice",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u1\nFN:Alice\nEND:VCARD",
            "etag": "\"v1-alice\"",
            "is_group": false
        },
        {
            "id": "c2", "uid": "u2",
            "full_name": "Bob",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u2\nFN:Bob\nEND:VCARD",
            "etag": "\"v1-bob\"",
            "is_group": false
        }
    ]);

    Mock::given(method("GET"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contacts_v1.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri());
    let s1 = pull_contacts(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s1.added, 2);
    assert_eq!(s1.updated, 0);
    assert_eq!(s1.deleted, 0);
    assert!(s1.commit_sha.is_some());

    let vcf1 = repo
        .read_file("sources/forwardemail/test-alias/contacts/default/u1.vcf")
        .unwrap();
    assert!(std::str::from_utf8(&vcf1).unwrap().contains("FN:Alice"));

    // --- Second pull: Alice's etag changed, Bob gone, new contact Carol ---
    let contacts_v2 = serde_json::json!([
        {
            "id": "c1", "uid": "u1",
            "full_name": "Alice Smith",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u1\nFN:Alice Smith\nEND:VCARD",
            "etag": "\"v2-alice\"",
            "is_group": false
        },
        {
            "id": "c3", "uid": "u3",
            "full_name": "Carol",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u3\nFN:Carol\nEND:VCARD",
            "etag": "\"v1-carol\"",
            "is_group": false
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contacts_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let s2 = pull_contacts(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s2.added, 1, "carol");
    assert_eq!(s2.updated, 1, "alice");
    assert_eq!(s2.deleted, 1, "bob");

    // Alice file now contains the updated vCard
    let vcf1b = repo
        .read_file("sources/forwardemail/test-alias/contacts/default/u1.vcf")
        .unwrap();
    assert!(std::str::from_utf8(&vcf1b).unwrap().contains("Alice Smith"));

    // Bob file is gone
    assert!(repo
        .read_file("sources/forwardemail/test-alias/contacts/default/u2.vcf")
        .is_err());

    // --- Third pull: no changes → no-op, no new commit ---
    let contacts_v3 = serde_json::json!([
        {
            "id": "c1", "uid": "u1",
            "full_name": "Alice Smith",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u1\nFN:Alice Smith\nEND:VCARD",
            "etag": "\"v2-alice\"",
            "is_group": false
        },
        {
            "id": "c3", "uid": "u3",
            "full_name": "Carol",
            "content": "BEGIN:VCARD\nVERSION:3.0\nUID:u3\nFN:Carol\nEND:VCARD",
            "etag": "\"v1-carol\"",
            "is_group": false
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contacts_v3))
        .mount(&server)
        .await;

    let s3 = pull_contacts(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert!(s3.is_noop(), "no changes should be a no-op: {:?}", s3);
    assert_eq!(s3.commit_sha, None);
}

#[tokio::test]
async fn sieve_pull_creates_then_detects_content_change() {
    let server = MockServer::start().await;
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(repo_dir.path()).unwrap();

    let list_v1 = serde_json::json!([
        {"id": "s1", "name": "filter1", "is_active": false, "is_valid": true}
    ]);
    let get_v1 = serde_json::json!({
        "id": "s1", "name": "filter1",
        "content": "require [\"fileinto\"]; if header :contains \"subject\" \"foo\" { fileinto \"Junk\"; }",
        "is_active": false, "is_valid": true,
        "required_capabilities": ["fileinto"],
        "security_warnings": [], "validation_errors": []
    });

    Mock::given(method("GET"))
        .and(path("/v1/sieve-scripts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/sieve-scripts/s1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(get_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri());
    let s1 = pull_sieve(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s1.added, 1);
    assert_eq!(s1.updated, 0);
    assert!(s1.commit_sha.is_some());

    let body = repo
        .read_file("sources/forwardemail/test-alias/sieve/filter1.sieve")
        .unwrap();
    assert!(std::str::from_utf8(&body)
        .unwrap()
        .contains("fileinto \"Junk\""));

    // Second pull with changed content
    let list_v2 = serde_json::json!([
        {"id": "s1", "name": "filter1", "is_active": true, "is_valid": true}
    ]);
    let get_v2 = serde_json::json!({
        "id": "s1", "name": "filter1",
        "content": "require [\"fileinto\"]; if header :contains \"subject\" \"bar\" { fileinto \"Trash\"; }",
        "is_active": true, "is_valid": true,
        "required_capabilities": ["fileinto"],
        "security_warnings": [], "validation_errors": []
    });
    Mock::given(method("GET"))
        .and(path("/v1/sieve-scripts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/sieve-scripts/s1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(get_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let s2 = pull_sieve(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s2.updated, 1);
    assert_eq!(s2.added, 0);
    let body2 = repo
        .read_file("sources/forwardemail/test-alias/sieve/filter1.sieve")
        .unwrap();
    assert!(std::str::from_utf8(&body2)
        .unwrap()
        .contains("fileinto \"Trash\""));
}
