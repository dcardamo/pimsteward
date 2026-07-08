//! Live stalwart sieve e2e — exercises the real `StalwartSieveBackend`
//! (PUTSCRIPT/GETSCRIPT/LISTSCRIPTS/SETACTIVE/DELETESCRIPT) against the
//! Stalwart server on saturn.
//!
//! Run with:
//! ```sh
//! STALWART_LIVE=1 STALWART_PASS="$(cat ~/.config/secrets/stalwart-password)" \
//!   cargo test -p pimsteward --test stalwart_sieve_live -- --include-ignored --nocapture
//! ```

use pimsteward::mcp::ManageSieveConfig;
use pimsteward::source::sieve::StalwartSieveBackend;
use pimsteward::source::traits::SieveBackend;

fn live_env() -> Option<(String, String, String)> {
    if std::env::var("STALWART_LIVE").ok().as_deref() != Some("1") {
        eprintln!("stalwart_sieve_live: skip — set STALWART_LIVE=1");
        return None;
    }
    let host = std::env::var("STALWART_HOST").unwrap_or_else(|_| "stalwart.example.test".into());
    let user = std::env::var("STALWART_USER").unwrap_or_else(|_| "dan@example.test".into());
    let pass = match std::env::var("STALWART_PASS") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => {
            eprintln!("stalwart_sieve_live: skip — STALWART_PASS unset/empty");
            return None;
        }
    };
    Some((host, user, pass.trim().to_string()))
}

fn backend(host: &str, user: &str, pass: &str) -> StalwartSieveBackend {
    StalwartSieveBackend::new(ManageSieveConfig {
        host: host.into(),
        port: 4190,
        user: user.into(),
        password: pass.into(),
    })
}

/// Unique script name so parallel/repeated runs don't collide.
fn unique_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pimsteward-live-{nanos}")
}

const SAMPLE: &str = "require [\"fileinto\"];\n# live test rule\nif header :contains \"subject\" \"pimsteward-live\" { fileinto \"Junk\"; }\n";

#[tokio::test]
#[ignore = "live: requires STALWART_LIVE=1 + STALWART_PASS"]
async fn stalwart_sieve_backend_crud_and_activate() {
    let Some((host, user, pass)) = live_env() else { return };
    let backend = backend(&host, &user, &pass);
    let name = unique_name();
    eprintln!("test script name: {name}");

    // Clean slate: ensure the script doesn't exist (idempotent delete).
    backend.delete_script(&name).await.expect("idempotent pre-delete");

    // PUT (create).
    let put = backend.put_script(&name, SAMPLE).await.expect("put_script");
    assert_eq!(put.name, name);
    assert!(put.is_valid);
    eprintln!("put OK");

    // GET — content must round-trip.
    let got = backend.get_script(&name).await.expect("get_script");
    assert_eq!(got.name, name);
    let content = got.content.expect("content present");
    assert!(
        content.contains("pimsteward-live"),
        "content round-trip lost body: {content}"
    );
    eprintln!("get OK");

    // LIST must include our script (not active yet).
    let list = backend.list_scripts().await.expect("list_scripts");
    assert!(
        list.iter().any(|s| s.name == name && !s.is_active),
        "script {name} must appear in list_scripts (inactive)"
    );
    eprintln!("list OK ({} scripts)", list.len());

    // ACTIVATE.
    backend.activate_script(&name).await.expect("activate_script");
    let active = backend.get_active().await.expect("get_active");
    assert_eq!(active.as_deref(), Some(name.as_str()), "active script must be ours");
    eprintln!("activate OK");

    // Re-list to confirm active flag flips.
    let list2 = backend.list_scripts().await.expect("list_scripts after activate");
    assert!(
        list2.iter().any(|s| s.name == name && s.is_active),
        "script {name} must be ACTIVE in list_scripts"
    );
    eprintln!("list-after-activate OK");

    // DELETE (idempotent).
    backend.delete_script(&name).await.expect("delete_script");
    backend.delete_script(&name).await.expect("idempotent second delete");
    eprintln!("delete OK");

    // Final list must NOT include our script.
    let list3 = backend.list_scripts().await.expect("final list_scripts");
    assert!(
        !list3.iter().any(|s| s.name == name),
        "deleted script {name} must be gone from list_scripts"
    );
    eprintln!("stalwart_sieve_live: full CRUD + activate round trip complete");
}