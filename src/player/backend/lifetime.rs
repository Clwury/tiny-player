pub(super) struct ShutdownOrder<Owner, Dependent> {
    owner: Option<Owner>,
    dependent: Option<Dependent>,
}

impl<Owner, Dependent> ShutdownOrder<Owner, Dependent> {
    pub(super) fn new(owner: Option<Owner>, dependent: Option<Dependent>) -> Self {
        Self { owner, dependent }
    }

    pub(super) fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    pub(super) fn owner_mut(&mut self) -> Option<&mut Owner> {
        self.owner.as_mut()
    }

    pub(super) fn dependent(&self) -> Option<&Dependent> {
        self.dependent.as_ref()
    }

    pub(super) fn dependent_mut(&mut self) -> Option<&mut Dependent> {
        self.dependent.as_mut()
    }
}

impl<Owner, Dependent> Drop for ShutdownOrder<Owner, Dependent> {
    fn drop(&mut self) {
        // The dependent may still refer to the owner's output while shutting down.
        // Preserve the pre-refactor presenter-before-backend release order.
        drop(self.dependent.take());
        drop(self.owner.take());
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    struct DropRecorder {
        name: &'static str,
        drops: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.name);
        }
    }

    #[test]
    fn shutdown_order_drops_dependent_before_owner() {
        let drops = Rc::new(RefCell::new(Vec::new()));
        let recorded_drops = Rc::clone(&drops);

        let presenter = DropRecorder {
            name: "presenter",
            drops: Rc::clone(&drops),
        };
        let backend = DropRecorder {
            name: "backend",
            drops,
        };

        drop(ShutdownOrder::new(Some(backend), Some(presenter)));

        assert_eq!(&*recorded_drops.borrow(), &["presenter", "backend"]);
    }
}
