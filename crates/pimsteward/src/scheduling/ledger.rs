//! Append-only sent-ledger for scheduling messages. One JSON line per
//! (uid, sequence, recipient, method) send. Restart-safe dedup + audit trail.

use crate::error::Error;
use crate::scheduling::model::Method;
use crate::store::Repo;
use serde::{Deserialize, Serialize};

const LEDGER_PATH: &str = "scheduling/sent.jsonl";

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Request => "REQUEST",
        Method::Cancel => "CANCEL",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentRecord {
    pub uid: String,
    pub sequence: u32,
    pub recipient: String,
    pub method: String,
    pub message_id: String,
    pub sent_at: String,
}

pub struct Ledger {
    records: Vec<SentRecord>,
}

impl Ledger {
    pub fn load(repo: &Repo) -> Result<Self, Error> {
        let records = match repo.read_file(LEDGER_PATH) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<SentRecord>(l).ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        Ok(Self { records })
    }

    pub fn already_sent(&self, uid: &str, sequence: u32, recipient: &str, method: Method) -> bool {
        let m = method_str(method);
        self.records.iter().any(|r| {
            r.uid == uid && r.sequence == sequence && r.recipient == recipient && r.method == m
        })
    }

    pub fn last_sequence(&self, uid: &str) -> Option<u32> {
        self.records.iter().filter(|r| r.uid == uid).map(|r| r.sequence).max()
    }

    /// Append a record to memory and flush the whole ledger to disk. The
    /// caller is responsible for committing via `Repo::commit_all`.
    pub fn record(
        &mut self,
        repo: &Repo,
        uid: &str,
        sequence: u32,
        recipient: &str,
        method: Method,
        message_id: &str,
    ) -> Result<(), Error> {
        self.records.push(SentRecord {
            uid: uid.to_string(),
            sequence,
            recipient: recipient.to_string(),
            method: method_str(method).to_string(),
            message_id: message_id.to_string(),
            sent_at: chrono::Utc::now().to_rfc3339(),
        });
        let mut body = String::new();
        for r in &self.records {
            body.push_str(&serde_json::to_string(r).map_err(|e| Error::config(e.to_string()))?);
            body.push('\n');
        }
        repo.write_file(LEDGER_PATH, body.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_dedup_and_last_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let mut led = Ledger::load(&repo).unwrap();
        assert!(!led.already_sent("u1", 0, "a@x", Method::Request));
        led.record(&repo, "u1", 0, "a@x", Method::Request, "mid-1").unwrap();
        assert!(led.already_sent("u1", 0, "a@x", Method::Request));
        assert!(!led.already_sent("u1", 1, "a@x", Method::Request));
        led.record(&repo, "u1", 2, "a@x", Method::Cancel, "mid-2").unwrap();
        assert_eq!(led.last_sequence("u1"), Some(2));

        let led2 = Ledger::load(&repo).unwrap();
        assert!(led2.already_sent("u1", 0, "a@x", Method::Request));
        assert_eq!(led2.last_sequence("u1"), Some(2));
    }
}
