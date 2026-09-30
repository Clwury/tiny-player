use crate::{
    effects::{RequestSlot, RequestToken},
    server::CachedServer,
};

/// Selection belongs to the server controller. Re-selection/cancel invalidates
/// the slot; the runner cancels its handle. Completions also compare credentials.
#[derive(Debug)]
pub(super) enum SelectionState {
    Idle,
    Authenticating {
        server_id: String,
        slot: Box<RequestSlot>,
    },
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthRequest {
    pub(crate) server: CachedServer,
    pub(crate) token: RequestToken,
}

pub(crate) enum ServerIntent {
    SelectServer(String),
    CancelSelection,
    ToggleAutoStart(String),
    ReorderServer {
        server_id: String,
        target_id: String,
    },
    BeginReorder(String),
    ReorderPreview(usize),
    CommitReorder(bool),
    OpenIconPicker(String),
    CloseIconPicker,
    SearchIcons(String),
    SelectIcon(usize),
    OpenMenu(String),
    CloseMenu,
}

pub(crate) enum ServerCommand {
    Ignored,
    Cancelled,
    Open(Box<CachedServer>),
    Authenticate(Box<AuthRequest>),
    CatalogChanged,
    PreviewChanged,
    DownloadIcon(Box<super::icons::IconRequest>),
}

pub(crate) enum AuthResult {
    Ignored,
    Invalidated,
    Ready(anyhow::Result<Box<CachedServer>>),
}

#[derive(Clone, Debug)]
pub(crate) struct CountRequest {
    pub(crate) server: CachedServer,
    pub(crate) token: RequestToken,
}

/// One submission belongs to the open dialog's scope. Closing/replacing the
/// dialog cancels it; only its current token may reach the save transaction.
pub(crate) struct SaveRequest {
    pub(crate) submission: crate::server::AddServerSubmission,
    pub(crate) existing: Option<CachedServer>,
    pub(crate) token: RequestToken,
}

pub(crate) enum CountResult {
    Ignored,
    Saved,
    Failed,
}

/// Drag intent changes preview only. Commit applies IDs to the current catalog,
/// so metadata received while dragging cannot be restored from a stale clone.
#[derive(Debug)]
pub(super) struct ReorderState {
    pub(super) server_id: String,
    pub(super) target_index: usize,
}
