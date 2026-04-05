//! e2e tests for sieve scripts: install/update/delete + restore.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::sieve::pull_sieve;
use pimsteward::restore;
use pimsteward::write;

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn sieve_install_update_delete_lifecycle() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e sieve lifecycle");
    let name = format!("e2e_test_{}", std::process::id());

    // Initial pull so the tree has current state
    let _ = pull_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("baseline");

    let v1_content =
        r#"require ["fileinto"]; if header :contains "subject" "foo" { fileinto "Junk"; }"#;
    let installed = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        v1_content,
    )
    .await
    .expect("install");
    assert_eq!(installed.name, name);
    assert!(installed.is_valid, "v1 content should be valid sieve");
    let script_id = installed.id.clone();

    // .sieve file lands in git
    let sieve_path = format!(
        "sources/forwardemail/{}/sieve/{}.sieve",
        ctx.alias_slug(),
        name
    );
    let body = String::from_utf8_lossy(&ctx.repo.read_file(&sieve_path).expect("sieve in repo"))
        .into_owned();
    assert!(body.contains("\"Junk\""));

    // Update
    let v2_content =
        r#"require ["fileinto"]; if header :contains "subject" "bar" { fileinto "Trash"; }"#;
    write::sieve::update_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script_id,
        v2_content,
    )
    .await
    .expect("update");
    let body2 =
        String::from_utf8_lossy(&ctx.repo.read_file(&sieve_path).expect("post-update sieve"))
            .into_owned();
    assert!(body2.contains("\"Trash\""));

    // Delete
    write::sieve::delete_sieve_script(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &script_id)
        .await
        .expect("delete");
    assert!(ctx.repo.read_file(&sieve_path).is_err());
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn sieve_restore_content_change() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e sieve restore");
    let name = format!("e2e_restore_{}", std::process::id());

    let good_content = r#"require ["fileinto"]; fileinto "Archive";"#;
    let script = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        good_content,
    )
    .await
    .expect("install");
    let script_id = script.id.clone();

    let good_sha = current_head(&ctx.repo);

    // Bad update
    let bad_content = r#"require ["fileinto"]; fileinto "Junk";"#;
    write::sieve::update_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script_id,
        bad_content,
    )
    .await
    .expect("bad update");

    // Restore
    let (plan, token) =
        restore::sieve::plan_sieve(&ctx.client, &ctx.repo, &ctx.alias_slug(), &name, &good_sha)
            .await
            .expect("plan");
    assert!(matches!(
        plan.operation,
        restore::sieve::SieveOperation::UpdateContent { .. }
    ));

    restore::sieve::apply_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    // Verify live content matches good_content
    let live = ctx
        .client
        .get_sieve_script(&script_id)
        .await
        .expect("fetch live");
    assert_eq!(live.content.as_deref(), Some(good_content));

    // Cleanup
    write::sieve::delete_sieve_script(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &script_id)
        .await
        .expect("cleanup");
}

fn current_head(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
