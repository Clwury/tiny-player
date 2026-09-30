use super::ServerController;
use crate::{
    effects::{RequestScope, RequestSlot, RequestToken},
    server::{CachedServer, icon::all_icons},
};
use anyhow::Result;
use std::sync::Arc;

/// Feature-owned picker data. Opening creates a scope, closing drops it; the
/// GPUI overlay owns only focus, scrolling, editor and preview asset lifetime.
#[derive(Debug)]
pub(super) struct IconPickerState {
    server_id: String,
    selected_url: Option<String>,
    query: String,
    matching_icons: Arc<[usize]>,
    error: Option<String>,
    request: RequestSlot,
    progress: IconProgress,
}

#[derive(Debug)]
enum IconProgress {
    Idle,
    Downloading(String),
    Saving(Box<IconRequest>),
}

#[derive(Clone, Debug)]
pub(crate) struct IconRequest {
    pub(crate) url: String,
    pub(crate) token: RequestToken,
}

pub(crate) enum IconDownloadResult {
    Ignored,
    Invalidated,
    Failed,
    Save(Box<CachedServer>),
}

pub(crate) struct IconPickerVm<'a> {
    pub(crate) server_id: &'a str,
    pub(crate) selected_url: Option<&'a str>,
    pub(crate) pending_url: Option<&'a str>,
    pub(crate) query: &'a str,
    pub(crate) matching_icons: &'a Arc<[usize]>,
    pub(crate) error: Option<&'a str>,
}

impl ServerController {
    pub(super) fn open_icon_picker(&mut self, id: &str) -> bool {
        if self.icon_picker.is_some() || self.selecting_server_id().is_some() {
            return false;
        }
        let Some(server) = self.catalog.servers.iter().find(|server| server.id == id) else {
            return false;
        };
        self.icon_picker = Some(IconPickerState {
            server_id: server.id.clone(),
            selected_url: server.icon_url.clone(),
            query: String::new(),
            matching_icons: (0..all_icons().len()).collect(),
            error: None,
            request: RequestSlot::new(RequestScope::ServerIcon, server.workspace_identity()),
            progress: IconProgress::Idle,
        });
        true
    }

    pub(crate) fn icon_picker(&self) -> Option<IconPickerVm<'_>> {
        self.icon_picker.as_ref().map(|picker| IconPickerVm {
            server_id: &picker.server_id,
            selected_url: picker.selected_url.as_deref(),
            pending_url: match &picker.progress {
                IconProgress::Idle => None,
                IconProgress::Downloading(url) => Some(url),
                IconProgress::Saving(request) => Some(&request.url),
            },
            query: &picker.query,
            matching_icons: &picker.matching_icons,
            error: picker.error.as_deref(),
        })
    }

    pub(super) fn search_icons(&mut self, query: String) -> bool {
        let Some(picker) = self.icon_picker.as_mut() else {
            return false;
        };
        let query = query.trim().to_lowercase();
        if picker.query == query {
            return false;
        }
        picker.matching_icons = all_icons()
            .iter()
            .enumerate()
            .filter_map(|(index, icon)| icon.name.to_lowercase().contains(&query).then_some(index))
            .collect();
        picker.query = query;
        true
    }

    pub(super) fn begin_icon_download(&mut self, index: usize) -> Option<IconRequest> {
        let picker = self.icon_picker.as_mut()?;
        if !matches!(picker.progress, IconProgress::Idle) {
            return None;
        }
        let url = all_icons().get(index)?.url.clone();
        picker.error = None;
        picker.progress = IconProgress::Downloading(url.clone());
        Some(IconRequest {
            url,
            token: picker.request.issue(),
        })
    }

    pub(crate) fn finish_icon_download(
        &mut self,
        request: &IconRequest,
        result: Result<()>,
    ) -> IconDownloadResult {
        let Some(picker) = self.icon_picker.as_mut() else {
            return IconDownloadResult::Ignored;
        };
        if !request.token.is_current(&picker.request)
            || !matches!(&picker.progress, IconProgress::Downloading(url) if url == &request.url)
        {
            return IconDownloadResult::Ignored;
        }
        let current = self
            .catalog
            .servers
            .iter()
            .find(|server| server.id == picker.server_id);
        if current.is_none_or(|server| !request.token.is_for(&server.workspace_identity())) {
            picker.request.invalidate();
            picker.progress = IconProgress::Idle;
            return IconDownloadResult::Invalidated;
        }
        picker.request.commit(&request.token);
        match result {
            Ok(()) => {
                // Counts and any other metadata received during the download
                // must survive the icon save. Never commit the open-time clone.
                let mut server = current.unwrap().clone();
                server.icon_url = Some(request.url.clone());
                server.icon_is_custom = true;
                picker.progress = IconProgress::Saving(Box::new(request.clone()));
                IconDownloadResult::Save(Box::new(server))
            }
            Err(error) => {
                picker.progress = IconProgress::Idle;
                picker.error = Some(format!("选择图标失败：{error}"));
                IconDownloadResult::Failed
            }
        }
    }

    /// Immediate persistence is transactional. A failed save leaves the catalog
    /// unchanged and returns the picker to idle; a success closes its scope.
    pub(crate) fn finish_icon_save(&mut self, request: &IconRequest, result: Result<()>) -> bool {
        let Some(picker) = self.icon_picker.as_mut() else {
            return false;
        };
        if !matches!(&picker.progress, IconProgress::Saving(current) if current.token == request.token)
        {
            return false;
        }
        match result {
            Ok(()) => self.icon_picker = None,
            Err(error) => {
                picker.progress = IconProgress::Idle;
                picker.error = Some(format!("选择图标失败：{error}"));
            }
        }
        true
    }
}
