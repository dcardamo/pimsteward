//! Restore: bring a resource's live state back to what it looked like at a
//! past point in time.
//!
//! v1 supports contacts only. The pattern generalises to other resources
//! and can be extended as needed.
//!
//! # Two-call safety dance
//!
//! The AI cannot restore destructively in a single call. Every restore is
//! structured as:
//!
//! 1. [`plan_contact`] — reads git for the historical state, compares to
//!    live, returns a [`RestorePlan`] describing exactly what will happen
//!    plus a deterministic [`plan_token`] derived from the plan's bytes.
//!    Nothing touches forwardemail yet.
//! 2. [`apply_contact`] — takes the plan + the token, re-computes the
//!    token from the plan, and refuses to execute if they don't match.
//!    Then carries out the plan and commits the result.
//!
//! The token binding means the AI can't dry-run a small plan and then
//! apply a different, larger one under the same "confirmed" mandate. If
//! the live state has changed since the dry-run, the apply path can
//! detect a race by comparing etags before each mutation.

pub mod contacts;

pub use contacts::{apply_contact, plan_contact, RestorePlan};

use crate::error::Error;
use sha2::{Digest, Sha256};

/// Compute a deterministic token from the plan's canonical JSON form. Any
/// change to the plan (different path, different at_sha, different ops,
/// ordering) produces a different token.
pub fn plan_token(plan: &impl serde::Serialize) -> Result<String, Error> {
    let canonical = serde_json::to_vec(plan)?;
    let mut h = Sha256::new();
    h.update(&canonical);
    Ok(hex::encode(h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Dummy {
        a: i32,
        b: String,
    }

    #[test]
    fn same_plan_same_token() {
        let p1 = Dummy {
            a: 1,
            b: "hi".into(),
        };
        let p2 = Dummy {
            a: 1,
            b: "hi".into(),
        };
        assert_eq!(plan_token(&p1).unwrap(), plan_token(&p2).unwrap());
    }

    #[test]
    fn different_plan_different_token() {
        let p1 = Dummy {
            a: 1,
            b: "hi".into(),
        };
        let p2 = Dummy {
            a: 2,
            b: "hi".into(),
        };
        assert_ne!(plan_token(&p1).unwrap(), plan_token(&p2).unwrap());
    }
}
