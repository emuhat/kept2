//! Manual threads — lightweight, many-to-many attention clusters.
//!
//! A `Thread` is a named bag of pointers to cells and bullets. Threads do
//! not own content; the timeline still owns every cell and bullet. Membership
//! is recorded as `(thread_id, ReferenceTarget, attached_at)` — reusing the
//! existing `ReferenceTarget` shape so we don't carry two pointer enums.

use uuid::Uuid;

use crate::cell::ReferenceTarget;

pub type ThreadId = Uuid;

/// A user-named cluster. `closed_at` is the canonical soft-state flag
/// (mirroring `Cell::closed_at` / `Bullet::closed_at`); closed threads
/// disappear from the sidebar unless the global "Show inactive" toggle
/// is on.
#[derive(Clone, Debug)]
pub struct Thread {
    pub id: ThreadId,
    pub title: String,
    pub created_at: i64,
    pub edited_at: i64,
    pub closed_at: Option<i64>,
}

/// One attachment of a thread to a target. `target` is `WholeCell` for
/// cell-level attachments and `Subtree { cell_id, bullet_id }` for
/// bullet-level. The DB encodes this as `bullet_id IS NULL` ↔ `WholeCell`.
#[derive(Clone, Copy, Debug)]
pub struct ThreadMembership {
    pub thread_id: ThreadId,
    pub target: ReferenceTarget,
    pub attached_at: i64,
}
