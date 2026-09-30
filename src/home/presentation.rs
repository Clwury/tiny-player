//! GPUI-only state. Business models never own these handles or cells.
use std::cell::Cell;

use gpui::ScrollHandle;

/// Workspace-owned cached entity and timer. The pure layout controller decides
/// when to mount/invalidate; release cancels the timer and drops the cached view.
#[derive(Debug)]
pub(super) struct HomeDashboardResources {
    pub(super) entity: gpui::Entity<super::HomeDashboard>,
    pub(super) resize_settle: crate::effects::EffectHandle<gpui::Task<()>>,
}

impl HomeDashboardResources {
    pub(super) fn new(entity: gpui::Entity<super::HomeDashboard>) -> Self {
        Self {
            entity,
            resize_settle: Default::default(),
        }
    }
}

/// Owned by its library/favorite/search view; layout writes columns and scroll
/// interaction writes offset. Retained across route returns, dropped with Home.
#[derive(Clone, Debug)]
pub(crate) struct GridPresentation {
    pub(crate) scroll_handle: ScrollHandle,
    pub(crate) grid_columns: Cell<usize>,
}

impl Default for GridPresentation {
    fn default() -> Self {
        Self {
            scroll_handle: ScrollHandle::new(),
            grid_columns: Cell::new(1),
        }
    }
}

/// HomeContent owns this alongside SearchState. Query resets reset the offset;
/// first route activation focuses once. Both are released with the workspace.
#[derive(Debug, Default)]
pub(crate) struct SearchPresentation {
    pub(crate) grid: GridPresentation,
    pub(crate) focused_once: bool,
}
