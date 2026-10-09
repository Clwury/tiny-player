use super::controller::PersonRequest;
use crate::{emby::MediaItem, home::gateway::HomeGateway};

pub(super) fn run_person(
    gateway: &(impl HomeGateway + ?Sized),
    request: &PersonRequest,
) -> anyhow::Result<MediaItem> {
    gateway.media_item(&request.person_id)
}
