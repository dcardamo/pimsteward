pub mod model;
pub mod change_feed;
pub mod watermark;
pub mod ledger;
pub mod plan;

use crate::error::Error;
use crate::scheduling::model::{Method, Outbound};
use crate::store::Repo;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Abstraction over sending one iMIP message + one notify mail, so the
/// orchestrator is unit-testable without network access.
#[async_trait]
pub trait Sender: Send + Sync {
    /// Send the iMIP message to `to`; return a message-id string.
    async fn send_imip(
        &self, to: &str, subject: &str, text_body: &str, payload: &str, method: &str,
    ) -> Result<String, Error>;
    /// Send a plaintext notify mail to the alias owner.
    async fn notify(&self, subject: &str, body: &str) -> Result<(), Error>;
}

/// Process one pull commit. Returns the number of iMIP messages sent.
/// Pre-watermark commits and pre-existing events are never processed.
pub async fn run_scheduling(
    repo: &Repo,
    commit_sha: &str,
    sender: &dyn Sender,
    organizer_self: &str,
    notify_on_send: bool,
    now: DateTime<Utc>,
) -> Result<usize, Error> {
    let Some(wm) = watermark::read_watermark(repo) else {
        watermark::ensure_watermark(repo, commit_sha)?;
        return Ok(0);
    };
    if !watermark::commit_is_after(repo, &wm, commit_sha) {
        return Ok(0);
    }

    let changes = change_feed::change_feed(repo, commit_sha)?;
    let mut ledger = ledger::Ledger::load(repo)?;
    let mut sent_count = 0usize;
    let mut dirty = false;

    for change in &changes {
        for mut outbound in plan::plan_change(change, organizer_self, now) {
            outbound.sequence = resolve_sequence(&outbound, &ledger);
            for recipient in &outbound.recipients {
                if ledger.already_sent(&outbound.uid, outbound.sequence, recipient, outbound.method) {
                    continue;
                }
                let payload = build_payload(&outbound, organizer_self);
                let (subject, text) = render(&outbound);
                let method = method_word(outbound.method);
                let mid = sender
                    .send_imip(recipient, &subject, &text, &payload, method)
                    .await?;
                ledger.record(repo, &outbound.uid, outbound.sequence, recipient, outbound.method, &mid)?;
                dirty = true;
                sent_count += 1;
                if notify_on_send {
                    let nsub = format!("[scheduling] {} → {recipient}: {}", method, outbound.summary);
                    let nbody = format!(
                        "Sent {method} for \"{}\" (uid {}, seq {}) to {recipient}.",
                        outbound.summary, outbound.uid, outbound.sequence
                    );
                    sender.notify(&nsub, &nbody).await?;
                }
            }
        }
    }

    if dirty {
        repo.commit_all(
            "pimsteward-scheduling",
            "scheduling@pimsteward.local",
            &format!("scheduling: sent {sent_count} iMIP message(s) for {commit_sha}"),
        )?;
    }
    Ok(sent_count)
}

fn resolve_sequence(outbound: &Outbound, ledger: &ledger::Ledger) -> u32 {
    let event_seq = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "SEQUENCE")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    match outbound.method {
        Method::Cancel => ledger
            .last_sequence(&outbound.uid)
            .map(|s| s + 1)
            .unwrap_or(event_seq + 1)
            .max(event_seq),
        Method::Request => event_seq,
    }
}

fn build_payload(outbound: &Outbound, organizer_self: &str) -> String {
    pimsteward_ical::imip::build_imip(
        &outbound.event_ics,
        method_word(outbound.method),
        outbound.sequence,
        organizer_self,
        &outbound.recipients,
    )
}

fn method_word(m: Method) -> &'static str {
    match m { Method::Request => "REQUEST", Method::Cancel => "CANCEL" }
}

fn render(outbound: &Outbound) -> (String, String) {
    let verb = match outbound.method { Method::Request => "Invitation", Method::Cancel => "Cancelled" };
    let subject = format!("{verb}: {}", outbound.summary);
    let when = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "DTSTART").unwrap_or_default();
    let loc = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "LOCATION").unwrap_or_default();
    let body = format!("{verb}: {}\nWhen: {when}\nWhere: {loc}\nOrganizer: dan@hld.ca\n", outbound.summary);
    (subject, body)
}

#[cfg(test)]
mod orchestrator_tests {
    use super::*;
    use crate::store::Repo;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubSender {
        sent: Mutex<Vec<(String, String)>>, // (to, method)
        notes: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Sender for StubSender {
        async fn send_imip(&self, to: &str, _s: &str, _t: &str, _p: &str, method: &str)
            -> Result<String, Error> {
            self.sent.lock().unwrap().push((to.into(), method.into()));
            Ok(format!("mid-{}", self.sent.lock().unwrap().len()))
        }
        async fn notify(&self, subject: &str, _b: &str) -> Result<(), Error> {
            self.notes.lock().unwrap().push(subject.into());
            Ok(())
        }
    }

    fn git(p: &std::path::Path, args: &[&str]) {
        std::process::Command::new("git").current_dir(p).args(args)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t")
            .env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t")
            .status().unwrap();
    }

    #[tokio::test]
    async fn sends_once_then_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let p = dir.path();
        repo.empty_commit("t","t@t","root").unwrap();
        let head = repo.empty_commit("t","t@t","wm").unwrap();
        watermark::ensure_watermark(&repo, &head).unwrap();

        std::fs::create_dir_all(p.join("cal/events")).unwrap();
        let ics = "BEGIN:VEVENT\r\nUID:e1\r\nSEQUENCE:0\r\nDTSTART:20990101T120000Z\r\nSUMMARY:Lunch\r\nORGANIZER;EMAIL=dan@hld.ca:mailto:dan@hld.ca\r\nATTENDEE;EMAIL=heather@hld.ca:mailto:heather@hld.ca\r\nEND:VEVENT\r\n";
        std::fs::write(p.join("cal/events/e1.ics"), ics).unwrap();
        git(p, &["add","-A"]); git(p, &["commit","-m","add e1"]);
        let sha = String::from_utf8(std::process::Command::new("git").current_dir(p)
            .args(["rev-parse","HEAD"]).output().unwrap().stdout).unwrap().trim().to_string();

        let stub = StubSender::default();
        let now: DateTime<Utc> = "2026-06-11T00:00:00Z".parse().unwrap();
        let n = run_scheduling(&repo, &sha, &stub, "dan@hld.ca", true, now).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(stub.sent.lock().unwrap().clone(), vec![("heather@hld.ca".into(),"REQUEST".into())]);
        assert_eq!(stub.notes.lock().unwrap().len(), 1);

        let n2 = run_scheduling(&repo, &sha, &stub, "dan@hld.ca", true, now).await.unwrap();
        assert_eq!(n2, 0);
        assert_eq!(stub.sent.lock().unwrap().len(), 1);
    }
}
