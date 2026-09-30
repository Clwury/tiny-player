use crate::{persistence::AppPersistence, server::CachedServer};

use super::{FeedRequest, FeedRequestKind, FeedResponse};
use crate::home::gateway::HomeGateway;

const HOME_ITEM_PAGE_LIMIT: u32 = 30;

/// The runner captures one immutable server/port. All results return to the
/// FeedController token gate, before images, optimistic overlays or notices.
pub(in crate::home) fn run_feed(
    gateway: &impl HomeGateway,
    persistence: &dyn AppPersistence,
    server: &CachedServer,
    request: &FeedRequest,
) -> FeedResponse {
    match &request.kind {
        FeedRequestKind::Snapshot => FeedResponse::Snapshot(
            persistence
                .load_home(server)
                .map(|snapshot| snapshot.map(Box::new)),
        ),
        FeedRequestKind::Views => FeedResponse::Views(gateway.user_views()),
        FeedRequestKind::Resume => FeedResponse::Resume(gateway.resume_items()),
        FeedRequestKind::Latest {
            view_id,
            item_types,
        } => FeedResponse::Latest(gateway.latest_items(view_id, item_types, HOME_ITEM_PAGE_LIMIT)),
    }
}
