//! Live e2e test for the Stalwart provider against the real server on
//! saturn (`stalwart.example.test`, valid Let's Encrypt cert — normal TLS
//! validation, connect by hostname).
//!
//! **Opt-in only.** Gated like the other live suites:
//! - `#[ignore]` so plain `cargo test` skips it.
//! - `STALWART_LIVE=1` must be set.
//! - Credentials: `STALWART_PASS` (the account password); `STALWART_HOST`
//!   overrides the hostname (default `stalwart.example.test`),
//!   `STALWART_USER` the account (default `dan@example.test`).
//!
//! What it proves:
//! 1. **The discovery fix** — `list_events` against Stalwart must NOT 404
//!    (the old `/dav/<user>/` discovery path did). A successful list is the
//!    key signal that the collection-URL mode works.
//! 2. A full calendar round trip via the provider's CalDAV writer:
//!    create → list (sees it) → delete → list (gone).
//!
//! Run on saturn:
//! ```sh
//! STALWART_LIVE=1 STALWART_HOST=stalwart.example.test \
//!   STALWART_PASS="$(cat ~/.config/secrets/stalwart-password)" \
//!   cargo test -p pimsteward --test stalwart_live -- --include-ignored
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use pimsteward::config::{Config, ProviderConfigs, StalwartConfig};
use pimsteward::provider::stalwart::StalwartProvider;
use pimsteward::provider::Provider;

const IMPLICIT_TLS_IMAP_PORT: u16 = 993;
const DAV_PORT: u16 = 8443;

/// Returns `(host, user, pass)` or `None` (with a skip message) if the
/// opt-in env isn't present.
fn live_env() -> Option<(String, String, String)> {
    if std::env::var("STALWART_LIVE").ok().as_deref() != Some("1") {
        eprintln!("stalwart_live: skip — set STALWART_LIVE=1 (and STALWART_PASS)");
        return None;
    }
    let host = std::env::var("STALWART_HOST").unwrap_or_else(|_| "stalwart.example.test".into());
    let user = std::env::var("STALWART_USER").unwrap_or_else(|_| "dan@example.test".into());
    let pass = match std::env::var("STALWART_PASS") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => {
            eprintln!("stalwart_live: skip — STALWART_PASS unset/empty");
            return None;
        }
    };
    Some((host, user, pass.trim().to_string()))
}

/// Build a `StalwartProvider` pointed at the live server. Credentials are
/// written to temp files because the config loads them from disk like in
/// production.
fn live_provider(
    host: &str,
    user: &str,
    pass: &str,
) -> (StalwartProvider, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let u = dir.path().join("u");
    let p = dir.path().join("p");
    std::fs::write(&u, user).unwrap();
    std::fs::write(&p, pass).unwrap();
    // Stalwart percent-encodes the `@` in the account path segment.
    let enc_user = user.replace('@', "%40");
    let cfg = Config {
        provider: ProviderConfigs {
            stalwart: Some(StalwartConfig {
                alias_user_file: Some(u),
                alias_password_file: Some(p),
                imap_host: host.into(),
                imap_port: IMPLICIT_TLS_IMAP_PORT,
                caldav_base_url: format!(
                    "https://{host}:{DAV_PORT}/dav/cal/{enc_user}/default"
                ),
                carddav_base_url: format!(
                    "https://{host}:{DAV_PORT}/dav/card/{enc_user}/default"
                ),
                ..StalwartConfig::default()
            }),
            ..ProviderConfigs::default()
        },
        ..Config::default()
    };
    (StalwartProvider::new(&cfg).expect("provider builds"), dir)
}

fn unique_uid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pimsteward-stalwart-live-{nanos}")
}

#[tokio::test]
#[ignore = "live: requires STALWART_LIVE=1 + STALWART_PASS"]
async fn calendar_list_and_round_trip() {
    let Some((host, user, pass)) = live_env() else {
        return;
    };
    let (provider, _dir) = live_provider(&host, &user, &pass);
    let cal_source = provider.build_calendar_source().unwrap().unwrap();
    let cal_writer = provider.build_calendar_writer().unwrap().unwrap();
    let collection = provider.caldav_collection_url().to_string();

    // 1) The discovery fix: listing must succeed (no 404 from a derived
    //    /dav/<user>/ path). This is the key proof.
    let before = cal_source
        .list_events(None)
        .await
        .expect("list_events must NOT 404 — proves the collection-URL discovery fix");
    eprintln!(
        "stalwart_live: list_events OK before create ({} events)",
        before.len()
    );

    // 2) Create an event via the CalDAV writer.
    let uid = unique_uid();
    let ical = format!(
        "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//pimsteward//stalwart-live//EN\r\n\
BEGIN:VEVENT\r\n\
UID:{uid}\r\n\
DTSTAMP:20260101T000000Z\r\n\
DTSTART:20260601T120000Z\r\n\
DTEND:20260601T130000Z\r\n\
SUMMARY:pimsteward stalwart live test\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n"
    );

    // Run the create+list checks WITHOUT panicking so cleanup always runs.
    // Collect a verdict, then delete, then assert the verdict.
    let created = cal_writer.create_event(&collection, &uid, &ical).await;
    let round_trip: Result<(), String> = match &created {
        Err(e) => Err(format!("create_event failed: {e}")),
        Ok(ev) => {
            if ev.uid.as_deref() != Some(uid.as_str()) {
                Err(format!("created uid mismatch: {:?}", ev.uid))
            } else {
                match cal_source.list_events(None).await {
                    Err(e) => Err(format!("re-list failed: {e}")),
                    Ok(after) => {
                        if after.iter().any(|e| e.uid.as_deref() == Some(uid.as_str())) {
                            eprintln!(
                                "stalwart_live: created event visible in list ({} events)",
                                after.len()
                            );
                            Ok(())
                        } else {
                            Err(format!("created event {uid} not found in list"))
                        }
                    }
                }
            }
        }
    };

    // 4) Delete (idempotent) — runs regardless of the verdict above.
    let del = cal_writer.delete_event(&collection, &uid, "").await;
    if let Err(e) = &del {
        eprintln!("stalwart_live: cleanup delete failed: {e}");
    }

    // Now surface any round-trip failure.
    round_trip.expect("calendar create→list round trip");
    del.expect("delete_event should succeed");

    // 5) Final list — the event must be gone.
    let final_list = cal_source.list_events(None).await.expect("final list ok");
    assert!(
        !final_list.iter().any(|e| e.uid.as_deref() == Some(uid.as_str())),
        "deleted event {uid} must be absent from list_events"
    );
    eprintln!("stalwart_live: round trip complete (create→list→delete→list)");
}
