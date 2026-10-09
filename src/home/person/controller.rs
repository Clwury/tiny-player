use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{MediaItem, UserItemData, VideoItemType},
    home::{
        library::controller::{LibraryController, LibraryIntent, LibraryTransition, LibraryVm},
        model::LoadState,
    },
};

#[derive(Debug)]
pub(in crate::home) struct PersonController {
    person_id: String,
    item: Option<MediaItem>,
    load: LoadState,
    slot: RequestSlot,
    pub(in crate::home) items: LibraryController,
}

#[derive(Clone, Debug)]
pub(in crate::home) struct PersonRequest {
    pub(in crate::home) person_id: String,
    pub(in crate::home) token: RequestToken,
    pub(in crate::home) user_data_revision: u64,
}

pub(in crate::home) struct PersonTransition {
    pub(in crate::home) metadata: Option<PersonRequest>,
    pub(in crate::home) items: LibraryTransition,
}

pub(in crate::home) struct PersonVm<'a> {
    pub(in crate::home) loaded: bool,
    pub(in crate::home) user_data: Option<&'a UserItemData>,
    pub(in crate::home) items: LibraryVm<'a>,
}

impl PersonController {
    pub(in crate::home) fn new(person_id: String, identity: WorkspaceIdentity) -> Self {
        Self {
            slot: RequestSlot::new(
                RequestScope::Person {
                    person_id: person_id.clone(),
                },
                identity.clone(),
            ),
            items: LibraryController::for_person(person_id.clone(), identity),
            person_id,
            item: None,
            load: LoadState::Idle,
        }
    }

    pub(in crate::home) fn view_model(&self) -> PersonVm<'_> {
        PersonVm {
            loaded: self.load == LoadState::Loaded,
            user_data: self.item.as_ref().and_then(|item| item.user_data.as_ref()),
            items: self.items.view_model(),
        }
    }

    pub(in crate::home) fn enter(&mut self, revision: u64) -> PersonTransition {
        let metadata = self.load.can_start().then(|| {
            self.load = LoadState::Loading;
            PersonRequest {
                person_id: self.person_id.clone(),
                token: self.slot.issue(),
                user_data_revision: revision,
            }
        });
        PersonTransition {
            metadata,
            items: self.items.dispatch(
                LibraryIntent::Open(vec![VideoItemType::Movie, VideoItemType::Series]),
                revision,
            ),
        }
    }

    pub(in crate::home) fn complete(
        &mut self,
        request: &PersonRequest,
        result: anyhow::Result<MediaItem>,
        identity: &WorkspaceIdentity,
    ) -> Option<Result<MediaItem, String>> {
        if request.person_id != self.person_id
            || !request.token.is_for(identity)
            || !self.slot.commit(&request.token)
        {
            return None;
        }
        let result = result.and_then(|item| {
            anyhow::ensure!(item.id == self.person_id, "人物信息与请求不匹配");
            anyhow::ensure!(
                item.item_type.as_deref() == Some("Person"),
                "返回的项目不是人物"
            );
            Ok(item)
        });
        Some(match result {
            Ok(item) => {
                self.item = Some(item.clone());
                self.load = LoadState::Loaded;
                Ok(item)
            }
            Err(error) => {
                self.load = LoadState::Failed;
                Err(error.to_string())
            }
        })
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
