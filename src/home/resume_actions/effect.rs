use super::controller::{ResumeCommand, ResumeItemAction, ResumeItemActionResponse};
use crate::home::gateway::HomeGateway;

pub(super) fn run_resume_action(
    gateway: &(impl HomeGateway + ?Sized),
    command: &ResumeCommand,
) -> anyhow::Result<ResumeItemActionResponse> {
    match command.action {
        ResumeItemAction::MarkPlayed => gateway
            .mark_item_played(&command.item_id)
            .map(ResumeItemActionResponse::MarkedPlayed),
        ResumeItemAction::HideFromResume => gateway
            .hide_item_from_resume(&command.item_id)
            .map(|_| ResumeItemActionResponse::HiddenFromResume),
    }
}
