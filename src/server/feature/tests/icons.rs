use super::*;
use crate::{
    images::{ImageRepository, ServerIconLoad, ServerIconRequest},
    server::icon::all_icons,
};
use std::sync::Arc;

fn open(controller: &mut ServerController, id: &str) {
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenIconPicker(id.into())),
        ServerCommand::PreviewChanged
    ));
}

fn select(controller: &mut ServerController, index: usize) -> IconRequest {
    let ServerCommand::DownloadIcon(request) = controller.dispatch(ServerIntent::SelectIcon(index))
    else {
        panic!("download expected")
    };
    *request
}

#[test]
fn reopened_picker_and_retry_reject_old_success_and_failure() {
    let mut controller = controller();
    open(&mut controller, "one");
    let old = select(&mut controller, 0);
    assert!(matches!(
        controller.dispatch(ServerIntent::SelectIcon(1)),
        ServerCommand::Ignored
    ));
    controller.dispatch(ServerIntent::CloseIconPicker);
    open(&mut controller, "one");
    let current = select(&mut controller, 1);
    for result in [Ok(()), Err(anyhow::anyhow!("late"))] {
        assert!(matches!(
            controller.finish_icon_download(&old, result),
            IconDownloadResult::Ignored
        ));
    }
    let vm = controller.icon_picker().unwrap();
    assert_eq!(vm.pending_url, Some(current.url.as_str()));
    assert!(vm.error.is_none());
    assert!(matches!(
        controller.finish_icon_download(&current, Err(anyhow::anyhow!("current"))),
        IconDownloadResult::Failed
    ));
    assert!(
        controller
            .icon_picker()
            .unwrap()
            .error
            .unwrap()
            .contains("current")
    );
    let retry = select(&mut controller, 1);
    assert!(matches!(
        controller.finish_icon_download(&current, Ok(())),
        IconDownloadResult::Ignored
    ));
    assert!(matches!(
        controller.finish_icon_download(&retry, Ok(())),
        IconDownloadResult::Save(_)
    ));
    assert!(matches!(
        controller.finish_icon_download(&retry, Ok(())),
        IconDownloadResult::Ignored
    ));
    assert!(!controller.finish_icon_save(&current, Ok(())));
    assert_eq!(
        controller.icon_picker().unwrap().pending_url,
        Some(retry.url.as_str())
    );
}

#[derive(Debug)]
struct FakeImages {
    fail: bool,
    requests: Mutex<Vec<(String, ServerIconLoad)>>,
}
impl ImageRepository<ServerIconRequest> for FakeImages {
    type Image = Arc<gpui::RenderImage>;
    fn load(&self, request: &ServerIconRequest) -> Result<Self::Image> {
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), request.load));
        anyhow::ensure!(!self.fail, "offline");
        Ok(crate::images::server_icons::tests::stub_icon())
    }
}

#[test]
fn icon_effect_and_save_failure_keep_original_then_merge_current_metadata_on_retry() {
    let mut controller = controller();
    open(&mut controller, "one");
    let images = FakeImages {
        fail: true,
        requests: Mutex::default(),
    };
    let request = select(&mut controller, 0);
    let result = effect::download_icon(&images, &request);
    assert!(matches!(
        controller.finish_icon_download(&request, result),
        IconDownloadResult::Failed
    ));
    assert_eq!(
        *images.requests.lock().unwrap(),
        [(request.url.clone(), ServerIconLoad::Cached)]
    );
    assert!(controller.catalog.servers[0].icon_url.is_none());
    let images = FakeImages {
        fail: false,
        ..images
    };
    let request = select(&mut controller, 1);
    let result = effect::download_icon(&images, &request);
    assert!(matches!(
        controller.finish_icon_download(&request, result),
        IconDownloadResult::Save(_)
    ));
    assert!(controller.finish_icon_save(&request, Err(anyhow::anyhow!("save failed"))));
    assert!(controller.icon_picker().unwrap().pending_url.is_none());
    assert!(controller.catalog.servers[0].icon_url.is_none());

    let request = select(&mut controller, 2);
    controller.catalog.servers[0].item_counts = Some(crate::server::CachedItemCounts {
        movie_count: 7,
        series_count: 8,
    });
    let IconDownloadResult::Save(server) =
        controller.finish_icon_download(&request, effect::download_icon(&images, &request))
    else {
        panic!("save expected")
    };
    assert_eq!(server.item_counts.as_ref().unwrap().series_count, 8);
    let (catalog, _) = controller.proposed_save(*server, true).unwrap();
    // This is the point at which the runner commits a successful transaction.
    assert!(controller.catalog.servers[0].icon_url.is_none());
    controller.persist_catalog(catalog, |_| Ok(())).unwrap();
    assert!(controller.finish_icon_save(&request, Ok(())));
    assert!(controller.icon_picker().is_none());
    assert_eq!(
        controller.catalog.servers[0].icon_url.as_deref(),
        Some(request.url.as_str())
    );
    assert!(controller.catalog.servers[0].icon_is_custom);
}

#[test]
fn deleted_or_reauthenticated_account_cannot_receive_downloaded_icon() {
    for delete in [false, true] {
        let mut controller = controller();
        open(&mut controller, "one");
        let request = select(&mut controller, 0);
        if delete {
            controller
                .persist_catalog(controller.proposed_delete("one").unwrap(), |_| Ok(()))
                .unwrap();
        } else {
            controller.catalog.servers[0].user_id = Some("changed".into());
        }
        assert!(matches!(
            controller.finish_icon_download(&request, Err(anyhow::anyhow!("stale"))),
            IconDownloadResult::Invalidated
        ));
        let vm = controller.icon_picker().unwrap();
        assert!(vm.pending_url.is_none() && vm.error.is_none());
        assert!(
            controller
                .catalog
                .servers
                .iter()
                .all(|server| server.icon_url.is_none())
        );
    }
}

#[test]
fn icon_search_normalizes_once_and_reuses_results_until_query_changes() {
    let mut controller = controller();
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenIconPicker("missing".into())),
        ServerCommand::Ignored
    ));
    open(&mut controller, "one");
    assert!(matches!(
        controller.dispatch(ServerIntent::SelectServer("two".into())),
        ServerCommand::Ignored
    ));
    let query = all_icons()[0].name.to_uppercase();
    controller.dispatch(ServerIntent::SearchIcons(format!("  {query}  ")));
    let vm = controller.icon_picker().unwrap();
    assert_eq!(vm.query, query.trim().to_lowercase());
    assert!(vm.matching_icons.contains(&0));
    let matches = vm.matching_icons.clone();
    controller.dispatch(ServerIntent::SearchIcons(query));
    assert!(Arc::ptr_eq(
        &matches,
        controller.icon_picker().unwrap().matching_icons
    ));
    controller.dispatch(ServerIntent::SearchIcons(
        "no icon matches this search".into(),
    ));
    assert!(controller.icon_picker().unwrap().matching_icons.is_empty());
    controller.dispatch(ServerIntent::CloseIconPicker);
    open(&mut controller, "three");
    assert!(controller.icon_picker().unwrap().query.is_empty());
    assert_eq!(
        controller.icon_picker().unwrap().matching_icons.len(),
        all_icons().len()
    );
}
