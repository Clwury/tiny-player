use super::*;

#[gpui::test]
fn reopening_loaded_library_reuses_scroll_and_sort_selection_closes_only_its_menu(
    cx: &mut gpui::TestAppContext,
) {
    let server = serde_json::from_value(serde_json::json!({
        "id":"local", "server_id":"remote", "user_id":"user",
        "endpoint":{"protocol":"Https", "address":"example.com", "port":443, "path":""},
        "username":"test", "password":"", "added_at_unix":0
    }))
    .unwrap();
    let page = cx.new(|cx| {
        HomeContent::new(
            server,
            crate::emby::EmbyClient::new("test".into()).unwrap(),
            cx,
        )
    });
    let view: UserView = serde_json::from_value(serde_json::json!({
        "Id":"library", "Name":"Movies", "CollectionType":"movies"
    }))
    .unwrap();
    page.update(cx, |page, cx| {
        let mut library = LibraryController::new(
            vec![VideoItemType::Movie],
            view.id.clone(),
            page.request_identity(),
        );
        library.test_paged_mut().initial = crate::home::LoadState::Loaded;
        page.controller
            .test_state_mut()
            .libraries
            .insert(view.id.clone(), library);
        page.open_library_for_view(&view, cx);
        let offset = gpui::point(gpui::px(0.0), gpui::px(-600.0));
        page.library_resources[&view.id]
            .presentation
            .grid
            .scroll_handle
            .set_offset(offset);
        page.toggle_current_library_sort_menu(cx);
        assert!(page.library_resources[&view.id].presentation.sort_menu_open);
        page.select_library_sort_by(view.id.clone(), UserItemsSort::SortName, cx);
        assert!(!page.library_resources[&view.id].presentation.sort_menu_open);
        assert_eq!(
            page.library_resources[&view.id]
                .presentation
                .grid
                .scroll_handle
                .offset(),
            offset
        );
        page.open_library_for_view(&view, cx);
        assert_eq!(page.library_resources.len(), 1);
        assert_eq!(
            page.library_resources[&view.id]
                .presentation
                .grid
                .scroll_handle
                .offset(),
            offset
        );
        assert_eq!(
            page.controller.test_state().libraries[&view.id]
                .view_model()
                .paged
                .initial,
            crate::home::LoadState::Loaded
        );
    });
    cx.run_until_parked();
}
