use super::model::{PlayedCommand, PlayedResponse};
use crate::home::gateway::HomeGateway;

pub(in crate::home) fn run_played(
    gateway: &(impl HomeGateway + ?Sized),
    command: &PlayedCommand,
) -> anyhow::Result<PlayedResponse> {
    let request = &command.request;
    let data = gateway.set_played(&request.item_id, request.played)?;
    // A single Episode changes the Series aggregate; read it after mutation.
    let parent = if !request.whole_series {
        request.series_id.as_ref().map(|id| gateway.media_item(id))
    } else {
        None
    };
    // Both followups run even when the earlier one fails.
    let episodes = request
        .whole_series
        .then(|| gateway.show_episodes(&request.item_id, request.season_id.as_deref()));
    let episode = if request.whole_series {
        request
            .episode_id
            .as_deref()
            .map(|id| gateway.media_item(id))
    } else {
        None
    };
    Ok(PlayedResponse {
        data,
        parent,
        episodes,
        episode,
    })
}
