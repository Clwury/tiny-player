//! Scripted mutation port for controller/effect tests. Responses are returned
//! in call order, so tests also detect skipped or unexpected follow-up reads.
use super::HomeGateway;
use crate::emby::{
    MediaItem, MediaItems, ResumeItems, UserItem, UserItemData, UserItems, UserItemsQuery,
    UserViews, VideoItemType,
};
use std::{collections::VecDeque, sync::Mutex};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Call {
    MarkPlayed(String),
    Hide(String),
    SetPlayed(String, bool),
    Item(String),
    Episodes(String, Option<String>),
    Similar(String),
    Seasons(String),
    NextUp(String),
    Sources(String),
}

pub(crate) enum Reply {
    UserData(anyhow::Result<UserItemData>),
    Hidden(anyhow::Result<()>),
    Item(Box<anyhow::Result<MediaItem>>),
    Episodes(anyhow::Result<MediaItems>),
    Similar(anyhow::Result<UserItems>),
    Seasons(anyhow::Result<MediaItems>),
    NextUp(anyhow::Result<MediaItems>),
    Sources(anyhow::Result<Vec<crate::emby::MediaSource>>),
}

impl Reply {
    pub(crate) fn item(value: anyhow::Result<MediaItem>) -> Self {
        Self::Item(Box::new(value))
    }
}

pub(crate) struct FakeMutations {
    pub(crate) calls: Mutex<Vec<Call>>,
    replies: Mutex<VecDeque<Reply>>,
}

impl FakeMutations {
    pub(crate) fn new(replies: Vec<Reply>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        }
    }
    fn reply(&self, call: Call) -> Reply {
        self.calls.lock().unwrap().push(call);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected gateway call")
    }
}

impl HomeGateway for FakeMutations {
    fn similar_items(&self, id: &str) -> anyhow::Result<crate::emby::UserItems> {
        let Reply::Similar(reply) = self.reply(Call::Similar(id.into())) else {
            panic!("expected similar items")
        };
        reply
    }
    fn show_seasons(&self, id: &str) -> anyhow::Result<crate::emby::MediaItems> {
        let Reply::Seasons(reply) = self.reply(Call::Seasons(id.into())) else {
            panic!("expected seasons")
        };
        reply
    }
    fn show_next_up(&self, id: &str) -> anyhow::Result<crate::emby::MediaItems> {
        let Reply::NextUp(reply) = self.reply(Call::NextUp(id.into())) else {
            panic!("expected next-up")
        };
        reply
    }
    fn playback_media_sources(&self, id: &str) -> anyhow::Result<Vec<crate::emby::MediaSource>> {
        let Reply::Sources(reply) = self.reply(Call::Sources(id.into())) else {
            panic!("expected sources")
        };
        reply
    }

    fn mark_item_played(&self, id: &str) -> anyhow::Result<UserItemData> {
        let Reply::UserData(reply) = self.reply(Call::MarkPlayed(id.into())) else {
            panic!("expected user data")
        };
        reply
    }
    fn hide_item_from_resume(&self, id: &str) -> anyhow::Result<()> {
        let Reply::Hidden(reply) = self.reply(Call::Hide(id.into())) else {
            panic!("expected hide result")
        };
        reply
    }
    fn set_played(&self, id: &str, played: bool) -> anyhow::Result<UserItemData> {
        let Reply::UserData(reply) = self.reply(Call::SetPlayed(id.into(), played)) else {
            panic!("expected user data")
        };
        reply
    }
    fn media_item(&self, id: &str) -> anyhow::Result<MediaItem> {
        let Reply::Item(reply) = self.reply(Call::Item(id.into())) else {
            panic!("expected media item")
        };
        *reply
    }
    fn show_episodes(&self, series: &str, season: Option<&str>) -> anyhow::Result<MediaItems> {
        let Reply::Episodes(reply) =
            self.reply(Call::Episodes(series.into(), season.map(str::to_owned)))
        else {
            panic!("expected episodes")
        };
        reply
    }
    fn set_favorite(&self, _: &str, _: bool) -> anyhow::Result<UserItemData> {
        panic!("unexpected favorite mutation")
    }
    fn user_views(&self) -> anyhow::Result<UserViews> {
        panic!("unexpected user views")
    }
    fn resume_items(&self) -> anyhow::Result<ResumeItems> {
        panic!("unexpected resume items")
    }
    fn latest_items(&self, _: &str, _: &[VideoItemType], _: u32) -> anyhow::Result<Vec<UserItem>> {
        panic!("unexpected latest items")
    }
    fn user_items(&self, _: &UserItemsQuery) -> anyhow::Result<UserItems> {
        panic!("unexpected user items")
    }
    fn search_items(&self, _: &str, _: u32, _: u32) -> anyhow::Result<UserItems> {
        panic!("unexpected search")
    }
}
