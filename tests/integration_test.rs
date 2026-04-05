//! Integration tests — wiremock for forwardemail, real git store, no
//! in-process mocks.

use pimsteward::forwardemail::Client;
use pimsteward::pull::calendar::pull_calendar;
use pimsteward::pull::contacts::pull_contacts;
use pimsteward::pull::mail::pull_mail;
use pimsteward::pull::sieve::pull_sieve;
use pimsteward::source::RestMailSource;
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

#[tokio::test]
async fn mail_pull_handles_create_flag_change_and_delete() {
    let server = MockServer::start().await;
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(repo_dir.path()).unwrap();

    // Folders list — one folder, INBOX
    let folders = serde_json::json!([
        {
            "id": "fold-1", "path": "INBOX", "name": "INBOX",
            "uid_validity": 1000, "uid_next": 5, "modify_index": 10,
            "subscribed": true, "special_use": "\\Inbox"
        }
    ]);

    // Helper: set up folder list response that can respond unlimited times
    Mock::given(method("GET"))
        .and(path("/v1/folders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(folders))
        .mount(&server)
        .await;

    // --- First pull: one message with modseq=5 ---
    let msgs_v1 = serde_json::json!([
        {
            "id": "m1", "folder_id": "fold-1", "folder_path": "INBOX",
            "subject": "hello", "size": 100, "uid": 1,
            "modseq": 5, "updated_at": "2026-04-05T10:00:00Z",
            "flags": []
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(msgs_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let msg_full_v1 = serde_json::json!({
        "id": "m1",
        "folder_path": "INBOX",
        "subject": "hello",
        "flags": [],
        "modseq": 5,
        "raw": "From: sender@example.com\r\nTo: alice@test.example\r\nSubject: hello\r\n\r\noriginal body",
        "nodemailer": {"text": "original body"}
    });
    Mock::given(method("GET"))
        .and(path("/v1/messages/m1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(msg_full_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri());
    let s1 = pull_mail(
        &RestMailSource::new(client.clone()),
        &repo,
        "test-alias",
        "test",
        "test@x",
    )
    .await
    .unwrap();
    assert_eq!(s1.added, 1, "first pull adds one message");
    assert!(s1.commit_sha.is_some());
    // raw RFC822 is written as <id>.eml
    let eml = repo
        .read_file("sources/forwardemail/test-alias/mail/INBOX/m1.eml")
        .unwrap();
    let eml_str = std::str::from_utf8(&eml).unwrap();
    assert!(eml_str.contains("Subject: hello"));
    assert!(eml_str.contains("original body"));

    // --- Second pull: same message, flags updated (modseq bumps) ---
    let msgs_v2 = serde_json::json!([
        {
            "id": "m1", "folder_id": "fold-1", "folder_path": "INBOX",
            "subject": "hello", "size": 100, "uid": 1,
            "modseq": 6, "updated_at": "2026-04-05T10:01:00Z",
            "flags": ["\\Seen", "\\Flagged"]
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(msgs_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let msg_full_v2 = serde_json::json!({
        "id": "m1",
        "flags": ["\\Seen", "\\Flagged"],
        "modseq": 6,
        "subject": "hello",
        "raw": "From: sender@example.com\r\nTo: alice@test.example\r\nSubject: hello\r\n\r\noriginal body",
        "nodemailer": {"text": "original body"}
    });
    Mock::given(method("GET"))
        .and(path("/v1/messages/m1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(msg_full_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let s2 = pull_mail(
        &RestMailSource::new(client.clone()),
        &repo,
        "test-alias",
        "test",
        "test@x",
    )
    .await
    .unwrap();
    assert_eq!(s2.updated, 1, "modseq change detected as update");
    assert_eq!(s2.added, 0);
    assert_eq!(s2.deleted, 0);

    // --- Third pull: message deleted on the server ---
    let msgs_v3 = serde_json::json!([]);
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(msgs_v3))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let s3 = pull_mail(
        &RestMailSource::new(client.clone()),
        &repo,
        "test-alias",
        "test",
        "test@x",
    )
    .await
    .unwrap();
    assert_eq!(s3.deleted, 1, "missing-from-remote detected as deletion");
    assert!(repo
        .read_file("sources/forwardemail/test-alias/mail/INBOX/m1.eml")
        .is_err());
}

#[tokio::test]
async fn calendar_pull_handles_events_with_raw_ical() {
    let server = MockServer::start().await;
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(repo_dir.path()).unwrap();

    let calendars = serde_json::json!([
        {
            "id": "cal-1", "name": "personal", "color": "#f00",
            "timezone": "America/Toronto"
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/calendars"))
        .respond_with(ResponseTemplate::new(200).set_body_json(calendars))
        .mount(&server)
        .await;

    // First pull: one event
    let events_v1 = serde_json::json!([
        {
            "id": "e1", "uid": "uid-1", "calendar_id": "cal-1",
            "ical": "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:uid-1\nSUMMARY:v1\nEND:VEVENT\nEND:VCALENDAR",
            "summary": "v1"
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/calendar-events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(events_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri());
    let s1 = pull_calendar(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s1.added, 1);
    let ics = repo
        .read_file("sources/forwardemail/test-alias/calendars/cal-1/events/uid-1.ics")
        .unwrap();
    assert!(std::str::from_utf8(&ics).unwrap().contains("SUMMARY:v1"));

    // Second pull: same event, etag changed → update
    let events_v2 = serde_json::json!([
        {
            "id": "e1", "uid": "uid-1", "calendar_id": "cal-1",
            "ical": "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:uid-1\nSUMMARY:v2\nEND:VEVENT\nEND:VCALENDAR",
            "summary": "v2"
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/calendar-events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(events_v2))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let s2 = pull_calendar(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s2.updated, 1);
    assert_eq!(s2.added, 0);
    let ics2 = repo
        .read_file("sources/forwardemail/test-alias/calendars/cal-1/events/uid-1.ics")
        .unwrap();
    assert!(std::str::from_utf8(&ics2).unwrap().contains("SUMMARY:v2"));

    // Third pull: event deleted
    Mock::given(method("GET"))
        .and(path("/v1/calendar-events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let s3 = pull_calendar(&client, &repo, "test-alias", "test", "test@x")
        .await
        .unwrap();
    assert_eq!(s3.deleted, 1);
    assert!(repo
        .read_file("sources/forwardemail/test-alias/calendars/cal-1/events/uid-1.ics")
        .is_err());
}
