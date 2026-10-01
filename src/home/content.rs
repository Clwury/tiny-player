//! Workspace view construction and navigation binding.
use super::*;

impl HomeContent {
    #[cfg(test)]
    pub(super) fn new(
        current_server: CachedServer,
        emby_client: EmbyClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let ports = super::test_support::ports(&current_server, &emby_client);
        Self::with_ports(current_server, emby_client, ports, cx)
    }

    pub(super) fn with_ports(
        current_server: CachedServer,
        emby_client: EmbyClient,
        ports: HomePorts,
        cx: &mut Context<Self>,
    ) -> Self {
        let persistence = crate::persistence::PersistenceService::get(cx);
        cx.observe_global::<PlaybackTrackPreferences>(|page, cx| {
            if page.sync_track_preferences(cx) {
                page.schedule_home_snapshot_save(cx);
                cx.notify();
            }
        })
        .detach();
        cx.on_app_quit(|page, cx| page.finish_home_snapshot_saves(cx))
            .detach();
        cx.on_release(|page, cx| page.finish_home_snapshot_saves(cx).detach())
            .detach();
        let search_input = cx.new(|cx| Editor::new("搜索电影或剧集", cx).search());
        cx.subscribe(&search_input, |page, _, event, cx| {
            page.on_search_input_event(event, cx);
        })
        .detach();
        let home_content = cx.weak_entity();
        let home_dashboard = cx.new(move |_| HomeDashboard {
            home_content,
            #[cfg(test)]
            render_count: 0,
        });
        let observed_home_dashboard = home_dashboard.clone();
        cx.observe_self(move |page, cx| {
            // The cached dashboard reads HomeContent state through a separate
            // entity, so invalidate it whenever visible Home state changes. A
            // route-only transition back to Home can reuse the warm dashboard
            // cache; later data notifications invalidate it normally.
            if page.layout.page_notified(page.controller.route()) {
                observed_home_dashboard.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
        let authentication_error = current_server
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .is_none()
            .then(|| "Emby 用户信息缺失，请返回服务器页重新登录".into());
        let identity = current_server.workspace_identity();
        let feed_effects = feed::binding::FeedEffects::new(identity.clone());
        let images = ImageController::new(identity.clone());
        let image_repository = std::sync::Arc::new(EmbyImageRepository {
            client: emby_client.clone(),
            server: current_server.clone(),
        });
        let layout = model::layout::HomeLayoutController::new(identity.clone());
        let notifications = HomeNotificationQueue::new(identity.clone());
        let controller = HomeController::new(identity);
        Self {
            current_server,
            emby_client,
            layout,
            dashboard: presentation::HomeDashboardResources::new(home_dashboard),
            authentication_error,
            controller,
            ports,
            feed_effects,
            user_views_carousel: CarouselState::default(),
            resume_items_carousel: CarouselState::default(),
            latest_carousels: HashMap::new(),
            item_context_menu: None,
            resume_effects: HashMap::new(),
            library_resources: HashMap::new(),
            favorites_presentation: FavoritesPresentation::new(),
            search_effect: EffectHandle::default(),
            search_presentation: SearchPresentation::default(),
            search_input,
            favorite_effect: EffectHandle::default(),
            played_effect: EffectHandle::default(),
            detail_resources: HashMap::new(),
            notifications,
            home_scroll_handle: ScrollHandle::new(),
            images,
            image_effects: HashMap::new(),
            image_repository,
            persistence,
            #[cfg(test)]
            snapshot_save_path: None,
            playback_refresh_task: EffectHandle::default(),
        }
    }

    pub(super) fn start_effects(&mut self, cx: &mut Context<Self>) {
        if self.authentication_error.is_some() {
            cx.notify();
            return;
        }
        self.load_home_snapshot_if_needed(cx);
    }

    pub(super) fn title(&self) -> SharedString {
        self.controller.title().to_owned().into()
    }

    pub(super) fn detail_view(&self) -> Option<detail::binding::DetailView<'_>> {
        detail::binding::detail_view(self.controller.detail_view(), &self.detail_resources)
    }

    #[cfg(test)]
    pub(super) fn install_detail_fixture(
        &mut self,
        fixture: Option<detail::test_fixture::DetailFixture>,
    ) {
        if let Some(current) = self.controller.test_state().navigation.detail() {
            self.detail_resources.remove(&current.id());
        }
        let controller = fixture.map(|fixture| {
            let (controller, resources) = fixture.into_parts();
            self.detail_resources.insert(controller.id(), resources);
            controller
        });
        self.controller
            .test_state_mut()
            .navigation
            .set_detail_fixture(controller);
    }

    pub(super) fn root(&self) -> HomeRoot {
        self.controller.root()
    }

    pub(super) fn select_root(
        &mut self,
        root: HomeRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.controller.route() == &HomeRoute::Root(root) {
            return false;
        }
        self.sync_previous_offsets();
        let change = self
            .controller
            .dispatch_navigation(controller::NavigationIntent::Root(root));
        if !change.changed {
            return false;
        }

        // Non-Home workspaces keep the last dashboard frame cached behind their
        // opaque overlay. Do not dirty that cache for the route-only Home update.
        self.layout.root_selected(root);

        // Keep domain data and view handles alive for the original two-frame
        // release boundary, after cancelling all retired continuations now.
        let retired = detail::binding::apply_navigation_change(change, &mut self.detail_resources);
        if !retired.0.is_empty() || !retired.1.is_empty() {
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, _| drop(retired));
                window.refresh();
            });
        }
        self.clear_all_notifications();
        self.item_context_menu = None;
        match root {
            HomeRoot::Home => self.start_effects(cx),
            HomeRoot::Favorites if self.authentication_error.is_none() => {
                self.enter_favorites_if_needed(cx)
            }
            HomeRoot::Search if self.authentication_error.is_none() => {
                if !self.search_presentation.focused_once {
                    let focus = self.search_input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    self.search_presentation.focused_once = true;
                }
            }
            HomeRoot::Favorites => {}
            HomeRoot::Search => {}
        }
        cx.emit(HomeContentEvent::TitleChanged);
        true
    }

    pub(super) fn request_identity(&self) -> WorkspaceIdentity {
        self.current_server.workspace_identity()
    }

    pub(super) fn sync_previous_offsets(&mut self) {
        self.favorites_presentation.sync_previous_offsets();
        self.user_views_carousel.sync_previous_offset();
        self.resume_items_carousel.sync_previous_offset();
        for carousel in self.latest_carousels.values_mut() {
            carousel.sync_previous_offset();
        }
        if let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        {
            detail.presentation.episodes_carousel.sync_previous_offset();
            detail.presentation.people_carousel.sync_previous_offset();
            detail.presentation.similar_carousel.sync_previous_offset();
        }
    }
}
