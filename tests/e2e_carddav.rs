//! e2e tests for the CardDAV read path against the real forwardemail
//! CardDAV server (`carddav.forwardemail.net`).
//!
//! Read-only: verifies list_contacts returns valid vCards with ETags.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::source::{ContactsSource, DavContactsSource};
use pimsteward::write;

fn carddav_source(ctx: &E2eContext) -> DavContactsSource {
    let pass = std::fs::read_to_string(
        std::env::var("PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE")
            .unwrap_or_else(|_| "/home/dan/.config/secrets/pimsteward-test-alias-password".into()),
    )
    .expect("reading password file")
    .trim()
    .to_string();

    DavContactsSource::new(
        "https://carddav.forwardemail.net",
        ctx.alias.clone(),
        pass,
    )
    .expect("build CardDAV source")
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn carddav_list_contacts_returns_vcards_with_etags() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e carddav");
    let source = carddav_source(&ctx);

    // Create a test contact via REST so we have something to read.
    let marker = format!("e2e-carddav-{}", std::process::id());
    let created = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &marker,
        &[("home", "carddav_test@example.com")],
    )
    .await
    .expect("create test contact");
    let contact_id = created.id.clone();

    // Read via CardDAV
    let contacts = source.list_contacts().await.expect("list_contacts");
    assert!(
        !contacts.is_empty(),
        "CardDAV should return at least one contact"
    );

    let found = contacts.iter().find(|c| c.full_name == marker);
    assert!(
        found.is_some(),
        "CardDAV should return the test contact '{marker}'"
    );

    let contact = found.unwrap();
    assert!(
        !contact.content.is_empty(),
        "CardDAV contact should have vCard content"
    );
    assert!(
        contact.content.contains("BEGIN:VCARD"),
        "content should be a vCard"
    );
    assert!(
        !contact.etag.is_empty(),
        "CardDAV contact should have an etag"
    );

    // Cleanup
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
    )
    .await
    .expect("cleanup");
}
