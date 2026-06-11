//! Shared types for the organizer-side scheduling engine.

/// What happened to an event file between two pull commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One changed event `.ics` between a pull commit and its parent.
#[derive(Debug, Clone)]
pub struct EventChange {
    pub kind: ChangeKind,
    /// Repo-relative path, e.g. "<cal_dir>/events/<key>.ics".
    pub rel_path: String,
    pub uid: String,
    /// New file contents (None for Deleted).
    pub new_ics: Option<String>,
    /// Previous file contents (None for Added).
    pub old_ics: Option<String>,
}

/// iTIP scheduling method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Request,
    Cancel,
}

/// A single iMIP message to send: a method, the event it concerns, the
/// recipients, and the sequence number to stamp.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub method: Method,
    pub uid: String,
    pub sequence: u32,
    pub recipients: Vec<String>,
    pub event_ics: String,
    pub summary: String,
}
