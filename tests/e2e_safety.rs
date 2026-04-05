//! Safety guardrail tests — verify the hard-fail behavior at the boundary
//! between e2e tests and the real API.
//!
//! These tests do NOT hit the network. They verify that the guard fires
//! for the wrong alias strings even when PIMSTEWARD_RUN_E2E is set.

use pimsteward::safety;

#[test]
fn guard_allows_test_marker_aliases() {
    // Positive path — these should not panic
    safety::assert_test_alias("dotfiles_mcp_test@purpose.dev");
    safety::assert_test_alias("e2e_test@example.com");
    safety::assert_test_alias("something_TEST_else@a.b");
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn guard_refuses_production_dan_at_hld_ca() {
    safety::assert_test_alias("dan@hld.ca");
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn guard_refuses_production_dan_at_cardamore_ca() {
    safety::assert_test_alias("dan@cardamore.ca");
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn guard_refuses_missing_at_sign() {
    safety::assert_test_alias("just_a_test_string");
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn guard_refuses_plain_address_without_marker() {
    safety::assert_test_alias("someone@example.com");
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn env_guard_refuses_production_repo_path() {
    let path = std::path::Path::new("/data/Backups/saturn/pimsteward/dan_hld_ca");
    safety::assert_test_environment("dotfiles_mcp_test@purpose.dev", path);
}

#[test]
#[should_panic(expected = "SAFETY GUARD TRIPPED")]
fn env_guard_refuses_var_lib_pimsteward() {
    let path = std::path::Path::new("/var/lib/pimsteward/data");
    safety::assert_test_environment("dotfiles_mcp_test@purpose.dev", path);
}

#[test]
fn env_guard_allows_tempdir() {
    let d = tempfile::tempdir().unwrap();
    safety::assert_test_environment("dotfiles_mcp_test@purpose.dev", d.path());
}
