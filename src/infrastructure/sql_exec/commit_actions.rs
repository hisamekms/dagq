//! Commit notifications shared by adapters. SQLite's hook precedes the
//! result of COMMIT, so its notifications are drained only after that statement
//! returned. A rollback hook cancels even a commit whose hook already ran.
use std::cell::RefCell;
use std::rc::{Rc, Weak};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CommitState {
    Open,
    Committing,
    Committed,
}

type Action = Box<dyn FnOnce()>;
struct Pending {
    next: usize,
    actions: Vec<(usize, CommitState, Action)>,
}
#[derive(Clone)]
pub(crate) struct CommitActions(Rc<RefCell<Pending>>);
thread_local! {
    static PENDING: RefCell<Vec<Weak<RefCell<Pending>>>> = const { RefCell::new(Vec::new()) };
}
impl CommitActions {
    pub(crate) fn new() -> Self {
        let pending = Rc::new(RefCell::new(Pending {
            next: 0,
            actions: Vec::new(),
        }));
        PENDING.with_borrow_mut(|all| {
            // Postgres drains at commit directly, without the SQLite flush.
            // Retire dead entries here too, including read-only transactions.
            all.retain(|pending| pending.strong_count() > 0);
            all.push(Rc::downgrade(&pending));
        });
        Self(pending)
    }
    /// Registration numbers keep a savepoint boundary stable even if an older
    /// transaction's completed notifications are drained while it is open.
    pub(crate) fn checkpoint(&self) -> usize {
        self.0.borrow().next
    }
    pub(crate) fn rollback_to(&self, checkpoint: usize) {
        self.0
            .borrow_mut()
            .actions
            .retain(|(id, _, _)| *id < checkpoint);
    }
    pub(crate) fn push(&self, action: Action) {
        let mut pending = self.0.borrow_mut();
        let id = pending.next;
        pending.next += 1;
        pending.actions.push((id, CommitState::Open, action));
    }
    pub(crate) fn follow(&self, next: impl Fn(CommitState) -> Option<CommitState>) {
        let mut pending = self.0.borrow_mut();
        pending
            .actions
            .retain_mut(|(_, state, _)| match next(*state) {
                Some(next) => {
                    *state = next;
                    true
                }
                None => false,
            });
    }
    pub(crate) fn committed(&self) {
        self.follow(|_| Some(CommitState::Committed));
        self.flush();
    }
    fn flush(&self) {
        let actions = {
            let mut pending = self.0.borrow_mut();
            let (ready, open): (Vec<_>, Vec<_>) = std::mem::take(&mut pending.actions)
                .into_iter()
                .partition(|(_, state, _)| *state != CommitState::Open);
            pending.actions = open;
            ready
        };
        for (_, _, action) in actions {
            action();
        }
    }
}
/// Drain commit notifications after leaving SQLite's commit statement. Its
/// rollback hook has by then canceled a failed commit. Called after committing
/// an adapter transaction and by the transcript read guard for legacy callers.
pub(crate) fn flush_after_commit() {
    let live = PENDING.with_borrow_mut(|all| {
        all.retain(|p| p.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
    });
    for pending in live {
        CommitActions(pending).flush();
    }
}

#[cfg(test)]
pub(crate) fn pending_actions() -> usize {
    PENDING.with_borrow(|all| {
        all.iter()
            .filter_map(Weak::upgrade)
            .map(|p| p.borrow().actions.len())
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_savepoint_checkpoint_survives_draining_an_older_commit() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let actions = CommitActions::new();
        let called = calls.clone();
        actions.push(Box::new(move || called.borrow_mut().push("old")));
        actions.follow(|_| Some(CommitState::Committing));
        let called = calls.clone();
        actions.push(Box::new(move || called.borrow_mut().push("outer")));
        let checkpoint = actions.checkpoint();
        let called = calls.clone();
        actions.push(Box::new(move || called.borrow_mut().push("inside")));
        flush_after_commit();
        assert_eq!(*calls.borrow(), ["old"]);
        actions.rollback_to(checkpoint);
        actions.committed();
        assert_eq!(*calls.borrow(), ["old", "outer"]);
    }
}
