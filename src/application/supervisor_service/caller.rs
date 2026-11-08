//! Scoped, thread-local labels of the supervisor's work for observation.
//! They carry no authority and do not propagate implicitly to threads.
//! EndBound captures a label and establishes a fresh scope on its sending
//! thread so that its transport can put the label in the request.
use std::{cell::Cell, marker::PhantomData, rc::Rc};

thread_local! {
    static CURRENT: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// The current thread's innermost work label.
pub fn current() -> Option<&'static str> {
    CURRENT.get()
}

/// Restores the previous label on drop, including unwinding. It cannot
/// move to another thread, where dropping it would restore the wrong state.
#[must_use]
pub struct Guard(Option<&'static str>, PhantomData<Rc<()>>);

pub fn scope(name: &'static str) -> Guard {
    Guard(CURRENT.replace(Some(name)), PhantomData)
}

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.set(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_scopes_restore() {
        assert_eq!(current(), None);
        let outer = scope("outer");
        assert_eq!(current(), Some("outer"));
        {
            let _inner = scope("inner");
            assert_eq!(current(), Some("inner"));
        }
        assert_eq!(current(), Some("outer"));
        drop(outer);
        assert_eq!(current(), None);
    }
}
