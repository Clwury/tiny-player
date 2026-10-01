use crate::home::presentation::GridPresentation;

/// HomeContent owns this alongside SearchState. Query resets reset the offset;
/// first route activation focuses once. Both are released with the workspace.
#[derive(Debug, Default)]
pub(in crate::home) struct SearchPresentation {
    pub(in crate::home) grid: GridPresentation,
    pub(in crate::home) focused_once: bool,
}
