// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod store_key;

use std::{
    collections::BTreeSet,
    convert::{TryFrom, TryInto},
    mem::size_of,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use futures_core::stream::Stream;
use futures_util::stream::{self, TryStreamExt};
use matrix_sdk_common::async_trait;
use ruma::{
    api::client::r0::message::get_message_events::Direction,
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MembershipState, RoomMemberEventContent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncMessageEvent, AnySyncRoomEvent,
        AnySyncStateEvent, EventType, Redact,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, MxcUri, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use self::store_key::{EncryptedEvent, StoreKey};
use super::{Result, RoomInfo, StateChanges, StateStore, StoreError, StoredTimelineSlice};
use crate::{
    deserialized_responses::{MemberEvent, SyncRoomEvent},
    media::{MediaRequest, UniqueKey},
};

#[derive(Debug, Serialize, Deserialize)]
pub enum DatabaseType {
    Unencrypted,
    Encrypted(store_key::EncryptedStoreKey),
}

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] store_key::Error),
}

impl From<TransactionError<SerializationError>> for StoreError {
    fn from(e: TransactionError<SerializationError>) -> Self {
        match e {
            TransactionError::Abort(e) => e.into(),
            TransactionError::Storage(e) => StoreError::Sled(e),
        }
    }
}

impl From<SerializationError> for StoreError {
    fn from(e: SerializationError) -> Self {
        match e {
            SerializationError::Json(e) => StoreError::Json(e),
            SerializationError::Encryption(e) => match e {
                store_key::Error::Random(e) => StoreError::Encryption(e.to_string()),
                store_key::Error::Serialization(e) => StoreError::Json(e),
                store_key::Error::Encryption(e) => StoreError::Encryption(e),
            },
        }
    }
}

const ENCODE_SEPARATOR: u8 = 0xff;

trait EncodeKey {
    fn encode(&self) -> Vec<u8>;
}

impl EncodeKey for &UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str) {
    fn encode(&self) -> Vec<u8> {
        [self.0.as_bytes(), &[ENCODE_SEPARATOR], self.1.as_bytes(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeKey for (&RoomId, BatchIdx) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            &self.1.to_be_bytes().to_vec(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, BatchIdx, usize) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            &self.1.to_be_bytes().to_vec(),
            &[ENCODE_SEPARATOR],
            &self.2.to_be_bytes().to_vec(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, &str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.3.as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for EventType {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

/// Get the value at `position` in encoded `key`.
///
/// The key must have been encoded with the `EncodeKey` trait. `position`
/// corresponds to the position in the tuple before the key was encoded. If it
/// wasn't encoded in a tuple, use `0`.
///
/// Returns `None` if there is no key at `position`.
pub fn decode_key_value(key: &[u8], position: usize) -> Option<String> {
    let values: Vec<&[u8]> = key.split(|v| *v == ENCODE_SEPARATOR).collect();

    values.get(position).map(|s| String::from_utf8_lossy(s).to_string())
}

#[derive(Clone, Copy, Default, PartialEq)]
struct BatchIdx(usize);

impl From<sled::IVec> for BatchIdx {
    fn from(item: sled::IVec) -> Self {
        Self(usize::from_be_bytes(
            item.as_ref().try_into().expect("The batch index wasn't properly encoded"),
        ))
    }
}

impl BatchIdx {
    fn next(self) -> Self {
        Self(self.0 + 1)
    }

    fn to_be_bytes(self) -> [u8; size_of::<usize>()] {
        self.0.to_be_bytes()
    }
}

impl From<BatchIdx> for sled::IVec {
    fn from(item: BatchIdx) -> Self {
        item.to_be_bytes()[..].into()
    }
}

struct ExpandableBatch {
    start_token_changed: bool,
    end_token_changed: bool,
    batch_idx: BatchIdx,
    current_position: usize,
}

#[derive(Clone, Copy)]
struct EventPosition {
    position: usize,
    batch_idx: BatchIdx,
}

impl From<sled::IVec> for EventPosition {
    fn from(item: sled::IVec) -> Self {
        let (first, second) = item.split_at(size_of::<usize>());

        let batch_idx = BatchIdx(usize::from_be_bytes(
            first.try_into().expect("The event position wasn't properly encoded"),
        ));
        let position = usize::from_be_bytes(
            second.try_into().expect("The event position wasn't properly encoded"),
        );

        Self { batch_idx, position }
    }
}

impl From<EventPosition> for sled::IVec {
    fn from(item: EventPosition) -> Self {
        [item.batch_idx.0.to_be_bytes(), item.position.to_be_bytes()].concat().into()
    }
}

#[derive(Clone)]
pub struct SledStore {
    path: Option<PathBuf>,
    pub(crate) inner: Db,
    store_key: Arc<Option<StoreKey>>,
    session: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    display_names: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_room_info: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
    room_user_receipts: Tree,
    room_event_receipts: Tree,
    media: Tree,
    custom: Tree,
    // Map an EventId to the batch index and position in the batch
    event_id_to_position: Tree,
    // List of batches and the events
    timeline_events: Tree,
    batch_idx_to_start_token: Tree,
    batch_idx_to_end_token: Tree,
    start_token_to_batch_idx_position: Tree,
    end_token_to_batch_idx_position: Tree,
    // Keep track of the highest batch index
    highest_batch_idx: Tree,
}

impl std::fmt::Debug for SledStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledStore").field("path", &"memory store").finish()
        }
    }
}

impl SledStore {
    fn open_helper(db: Db, path: Option<PathBuf>, store_key: Option<StoreKey>) -> Result<Self> {
        let session = db.open_tree("session")?;
        let account_data = db.open_tree("account_data")?;

        let members = db.open_tree("members")?;
        let profiles = db.open_tree("profiles")?;
        let display_names = db.open_tree("display_names")?;
        let joined_user_ids = db.open_tree("joined_user_ids")?;
        let invited_user_ids = db.open_tree("invited_user_ids")?;

        let room_state = db.open_tree("room_state")?;
        let room_info = db.open_tree("room_infos")?;
        let presence = db.open_tree("presence")?;
        let room_account_data = db.open_tree("room_account_data")?;

        let stripped_room_info = db.open_tree("stripped_room_info")?;
        let stripped_members = db.open_tree("stripped_members")?;
        let stripped_room_state = db.open_tree("stripped_room_state")?;

        let room_user_receipts = db.open_tree("room_user_receipts")?;
        let room_event_receipts = db.open_tree("room_event_receipts")?;

        let media = db.open_tree("media")?;

        let custom = db.open_tree("custom")?;

        let event_id_to_position = db.open_tree("event_id_to_position")?;
        let timeline_events = db.open_tree("events")?;
        let batch_idx_to_start_token = db.open_tree("batch_idx_to_start_token")?;
        let batch_idx_to_end_token = db.open_tree("batch_idx_to_end_token")?;
        let start_token_to_batch_idx_position =
            db.open_tree("start_token_to_batch_idx_position")?;
        let end_token_to_batch_idx_position = db.open_tree("end_token_to_batch_idx_position")?;
        let highest_batch_idx = db.open_tree("highest_batch_idx")?;

        Ok(Self {
            path,
            inner: db,
            store_key: store_key.into(),
            session,
            account_data,
            members,
            profiles,
            display_names,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_room_info,
            stripped_members,
            stripped_room_state,
            room_user_receipts,
            room_event_receipts,
            media,
            custom,
            event_id_to_position,
            timeline_events,
            batch_idx_to_start_token,
            batch_idx_to_end_token,
            start_token_to_batch_idx_position,
            end_token_to_batch_idx_position,
            highest_batch_idx,
        })
    }

    pub fn open() -> Result<Self> {
        let db = Config::new().temporary(true).open()?;

        SledStore::open_helper(db, None, None)
    }

    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

        let store_key: Option<DatabaseType> = db
            .get("store_key".encode())?
            .map(|k| serde_json::from_slice(&k).map_err(StoreError::Json))
            .transpose()?;

        let store_key = if let Some(key) = store_key {
            if let DatabaseType::Encrypted(k) = key {
                StoreKey::import(passphrase, k).map_err(|_| StoreError::StoreLocked)?
            } else {
                return Err(StoreError::UnencryptedStore);
            }
        } else {
            let key = StoreKey::new().map_err::<StoreError, _>(|e| e.into())?;
            let encrypted_key = DatabaseType::Encrypted(
                key.export(passphrase).map_err::<StoreError, _>(|e| e.into())?,
            );
            db.insert("store_key".encode(), serde_json::to_vec(&encrypted_key)?)?;
            key
        };

        SledStore::open_helper(db, Some(path), Some(store_key))
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

        SledStore::open_helper(db, Some(path), None)
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<Vec<u8>, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted = key.encrypt(event)?;
            Ok(serde_json::to_vec(&encrypted)?)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: &[u8],
    ) -> Result<T, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted: EncryptedEvent = serde_json::from_slice(event)?;
            Ok(key.decrypt(encrypted)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session.insert(("filter", filter_name).encode(), filter_id)?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .session
            .get(("filter", filter_name).encode())?
            .map(|f| String::from_utf8_lossy(&f).to_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self
            .session
            .get("sync_token".encode())?
            .map(|t| String::from_utf8_lossy(&t).to_string()))
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        let ret: Result<(), TransactionError<SerializationError>> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
            &self.stripped_room_info,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    profiles,
                    display_names,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    presence,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    if let Some(s) = &changes.sync_token {
                        session.insert("sync_token".encode(), s.as_str())?;
                    }

                    for (room, events) in &changes.members {
                        let profile_changes = changes.profiles.get(room);

                        for event in events.values() {
                            let key = (room.as_str(), event.state_key.as_str()).encode();

                            match event.content.membership {
                                MembershipState::Join => {
                                    joined.insert(key.as_slice(), event.state_key.as_str())?;
                                    invited.remove(key.as_slice())?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(key.as_slice(), event.state_key.as_str())?;
                                    joined.remove(key.as_slice())?;
                                }
                                _ => {
                                    joined.remove(key.as_slice())?;
                                    invited.remove(key.as_slice())?;
                                }
                            }

                            members.insert(
                                key.as_slice(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(&event.state_key))
                            {
                                profiles.insert(
                                    key.as_slice(),
                                    self.serialize_event(&profile)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                        for (display_name, map) in ambiguity_maps {
                            display_names.insert(
                                (room_id.as_str(), display_name.as_str()).encode(),
                                self.serialize_event(&map)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (event_type, event) in &changes.account_data {
                        account_data.insert(
                            event_type.as_str().encode(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                (room.as_str(), event_type.as_str()).encode(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                state.insert(
                                    (room.as_str(), event_type.as_str(), state_key.as_str())
                                        .encode(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            room_id.encode(),
                            self.serialize_event(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            sender.encode(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room_id, info) in &changes.invited_room_info {
                        striped_rooms.insert(
                            room_id.encode(),
                            self.serialize_event(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            stripped_members.insert(
                                (room.as_str(), event.state_key.as_str()).encode(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                stripped_state.insert(
                                    (room.as_str(), event_type.as_str(), state_key.as_str())
                                        .encode(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        let ret: Result<(), TransactionError<SerializationError>> = (
            &self.room_user_receipts,
            &self.room_event_receipts,
            &self.event_id_to_position,
            &self.timeline_events,
            &self.batch_idx_to_start_token,
            &self.batch_idx_to_end_token,
            &self.start_token_to_batch_idx_position,
            &self.end_token_to_batch_idx_position,
            &self.highest_batch_idx,
            &self.room_info,
        )
            .transaction(
                |(
                    room_user_receipts,
                    room_event_receipts,
                    event_id_to_position,
                    timeline_events,
                    batch_idx_to_start_token,
                    batch_idx_to_end_token,
                    start_token_to_batch_idx_position,
                    end_token_to_batch_idx_position,
                    highest_batch_idx,
                    room_info,
                )| {
                    for (room, content) in &changes.receipts {
                        for (event_id, receipts) in &content.0 {
                            for (receipt_type, receipts) in receipts {
                                for (user_id, receipt) in receipts {
                                    // Add the receipt to the room user receipts
                                    if let Some(old) = room_user_receipts.insert(
                                        (room.as_str(), receipt_type.as_ref(), user_id.as_str())
                                            .encode(),
                                        self.serialize_event(&(event_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )? {
                                        // Remove the old receipt from the room event receipts
                                        let (old_event, _): (EventId, Receipt) = self
                                            .deserialize_event(&old)
                                            .map_err(ConflictableTransactionError::Abort)?;
                                        room_event_receipts.remove(
                                            (
                                                room.as_str(),
                                                receipt_type.as_ref(),
                                                old_event.as_str(),
                                                user_id.as_str(),
                                            )
                                                .encode(),
                                        )?;
                                    }

                                    // Add the receipt to the room event receipts
                                    room_event_receipts.insert(
                                        (
                                            room.as_str(),
                                            receipt_type.as_ref(),
                                            event_id.as_str(),
                                            user_id.as_str(),
                                        )
                                            .encode(),
                                        self.serialize_event(receipt)
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                }
                            }
                        }
                    }

                    for (room, timeline) in &changes.timeline {
                        let events: Vec<&SyncRoomEvent> = timeline
                            .events
                            .iter()
                            .filter(|event| event.event_id().is_some())
                            .collect();

                        let expandable = {
                            // Handle overlapping slices.
                            let (first_event, last_event) =
                                if let (Some(first_event), Some(last_event)) =
                                    (events.first(), events.last())
                                {
                                    (
                                        first_event.event_id().unwrap(),
                                        last_event.event_id().unwrap(),
                                    )
                                } else {
                                    continue;
                                };

                            let found_first = event_id_to_position
                                .get((room.as_str(), first_event.as_str()).encode())?
                                .map(EventPosition::from)
                                .map(|batch| ExpandableBatch {
                                    start_token_changed: false,
                                    end_token_changed: true,
                                    batch_idx: batch.batch_idx,
                                    current_position: batch.position,
                                });

                            let found_last = event_id_to_position
                                .get((room.as_str(), last_event.as_str()).encode())?
                                .map(EventPosition::from)
                                .map(|batch| {
                                    let current_position = batch.position - events.len();

                                    ExpandableBatch {
                                        start_token_changed: true,
                                        end_token_changed: false,
                                        batch_idx: batch.batch_idx,
                                        current_position,
                                    }
                                });

                            found_first.or(found_last)
                        };

                        // Lookup the previous or next batch
                        let expandable = if let Some(expandable) = expandable {
                            Some(expandable)
                        } else if let Some(batch) = end_token_to_batch_idx_position
                            .remove((room.as_str(), timeline.start.as_str()).encode())?
                            .map(EventPosition::from)
                        {
                            Some(ExpandableBatch {
                                start_token_changed: false,
                                end_token_changed: true,
                                batch_idx: batch.batch_idx,
                                current_position: batch.position,
                            })
                        } else if let Some(end) = &timeline.end {
                            if let Some(batch) = start_token_to_batch_idx_position
                                .remove((room.as_str(), end.as_str()).encode())?
                                .map(EventPosition::from)
                            {
                                let current_position = batch.position - events.len();
                                Some(ExpandableBatch {
                                    start_token_changed: true,
                                    end_token_changed: false,
                                    batch_idx: batch.batch_idx,
                                    current_position,
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        let expandable = if let Some(expandable) = expandable {
                            expandable
                        } else {
                            // If no expandable batch was found we add a new batch to the store
                            let batch_idx = highest_batch_idx
                                .get(room.as_str().encode())?
                                .map(BatchIdx::from)
                                .map_or(BatchIdx::default(), BatchIdx::next);

                            highest_batch_idx.insert(room.as_str().encode(), batch_idx)?;

                            ExpandableBatch {
                                start_token_changed: true,
                                end_token_changed: true,
                                batch_idx,
                                current_position: usize::MAX / 2,
                            }
                        };

                        // Remove events already known from the store
                        for event in &events {
                            if let Some(batch) = event_id_to_position
                                .remove(
                                    (room.as_str(), event.event_id().unwrap().as_str()).encode(),
                                )?
                                .map(EventPosition::from)
                            {
                                timeline_events.remove(
                                    (room.as_str(), batch.batch_idx, batch.position).encode(),
                                )?;
                            }
                        }

                        for (position, event) in events.iter().enumerate() {
                            let position = position + expandable.current_position;

                            let old_event = timeline_events.insert(
                                (room.as_str(), expandable.batch_idx, position).encode(),
                                self.serialize_event(event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;

                            if old_event.is_none() {
                                event_id_to_position.insert(
                                    (room.as_str(), event.event_id().unwrap().as_str()).encode(),
                                    EventPosition { batch_idx: expandable.batch_idx, position },
                                )?;
                            }

                            // Redact events
                            if let Ok(AnySyncRoomEvent::Message(
                                AnySyncMessageEvent::RoomRedaction(redaction),
                            )) = event.event.deserialize()
                            {
                                if let Some(batch) = event_id_to_position
                                    .get((room.as_str(), redaction.redacts.as_str()).encode())?
                                    .map(EventPosition::from)
                                {
                                    if let Some(full_event) = timeline_events
                                        .get(
                                            (room.as_str(), batch.batch_idx, batch.position)
                                                .encode(),
                                        )?
                                        .and_then(|t| {
                                            self.deserialize_event::<SyncRoomEvent>(&t).ok()
                                        })
                                        .and_then(|e| e.event.deserialize().ok())
                                    {
                                        let room_version = room_info
                                            .get(room.encode())?
                                            .map(|r| self.deserialize_event::<RoomInfo>(&r))
                                            .transpose().map(|i|i
                                                .and_then(|info| info.base_info.create.map(|event| event.room_version)));
                                        if let Ok(Some(room_version)) = room_version
                                        {
                                            let redacted_event: AnySyncRoomEvent =
                                                full_event.redact(redaction, &room_version).into();
                                            timeline_events.insert(
                                                (room.as_str(), batch.batch_idx, batch.position)
                                                    .encode(),
                                                self.serialize_event(&redacted_event)
                                                    .map_err(ConflictableTransactionError::Abort)?,
                                            )?;
                                        } else {
                                            warn!(
                                                "Was unable to find the room version for {}",
                                                room
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        if expandable.start_token_changed {
                            batch_idx_to_start_token.insert(
                                (room, expandable.batch_idx).encode(),
                                timeline.start.as_str().encode(),
                            )?;

                            start_token_to_batch_idx_position.insert(
                                (room.as_str(), timeline.start.as_str()).encode(),
                                EventPosition {
                                    batch_idx: expandable.batch_idx,
                                    position: expandable.current_position,
                                },
                            )?;
                        }

                        if expandable.end_token_changed {
                            if let Some(end) = &timeline.end {
                                batch_idx_to_end_token.insert(
                                    (room, expandable.batch_idx).encode(),
                                    end.as_str().encode(),
                                )?;

                                end_token_to_batch_idx_position.insert(
                                    (room.as_str(), end.as_str()).encode(),
                                    EventPosition {
                                        batch_idx: expandable.batch_idx,
                                        position: expandable.current_position + events.len(),
                                    },
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        let db = self.clone();
        let key = user_id.encode();
        spawn_blocking(move || {
            Ok(db.presence.get(key)?.map(|e| db.deserialize_event(&e)).transpose()?)
        })
        .await?
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str(), state_key).encode();
        spawn_blocking(move || {
            Ok(db.room_state.get(key)?.map(|e| db.deserialize_event(&e)).transpose()?)
        })
        .await?
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str()).encode();
        spawn_blocking(move || {
            Ok(db
                .room_state
                .scan_prefix(key)
                .flat_map(|e| e.map(|(_, e)| db.deserialize_event(&e)))
                .collect::<Result<_, _>>()?)
        })
        .await?
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        let db = self.clone();
        let key = (room_id.as_str(), user_id.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.profiles.get(key)?.map(|p| db.deserialize_event(&p)).transpose()?)
        })
        .await?
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        let db = self.clone();
        let key = (room_id.as_str(), state_key.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.members.get(key)?.map(|v| db.deserialize_event(&v)).transpose()?)
        })
        .await?
    }

    pub async fn get_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let decode = |key: &[u8]| -> Result<UserId> {
            let mut iter = key.split(|c| c == &ENCODE_SEPARATOR);
            // Our key is a the room id separated from the user id by a null
            // byte, discard the first value of the split.
            iter.next();

            let user_id = iter.next().expect("User ids weren't properly encoded");

            Ok(UserId::try_from(String::from_utf8_lossy(user_id).to_string())?)
        };

        let members = self.members.clone();
        let key = room_id.encode();

        spawn_blocking(move || stream::iter(members.scan_prefix(key).map(move |u| decode(&u?.0))))
            .await
            .map_err(Into::into)
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let db = self.clone();
        let key = room_id.encode();
        spawn_blocking(move || {
            stream::iter(db.invited_user_ids.scan_prefix(key).map(|u| {
                UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let db = self.clone();
        let key = room_id.encode();
        spawn_blocking(move || {
            stream::iter(db.joined_user_ids.scan_prefix(key).map(|u| {
                UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(
                db.room_info.iter().map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
            )
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_stripped_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(
                db.stripped_room_info
                    .iter()
                    .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
            )
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        let db = self.clone();
        let key = (room_id.as_str(), display_name).encode();
        spawn_blocking(move || {
            Ok(db
                .display_names
                .get(key)?
                .map(|m| db.deserialize_event(&m))
                .transpose()?
                .unwrap_or_default())
        })
        .await?
    }

    pub async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let db = self.clone();
        let key = event_type.encode();
        spawn_blocking(move || {
            Ok(db.account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.room_account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(EventId, Receipt)>> {
        let db = self.clone();
        let key = (room_id.as_str(), receipt_type.as_ref(), user_id.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.room_user_receipts.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
        let db = self.clone();
        let key = (room_id.as_str(), receipt_type.as_ref(), event_id.as_str()).encode();
        spawn_blocking(move || {
            db.room_event_receipts
                .scan_prefix(key)
                .map(|u| {
                    u.map_err(StoreError::Sled).and_then(|(key, value)| {
                        db.deserialize_event(&value)
                            // TODO remove this unwrapping
                            .map(|receipt| {
                                (decode_key_value(&key, 3).unwrap().try_into().unwrap(), receipt)
                            })
                            .map_err(Into::into)
                    })
                })
                .collect()
        })
        .await?
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.media.insert(
            (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
                .encode(),
            data,
        )?;

        self.inner.flush_async().await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let db = self.clone();
        let key = (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
            .encode();

        spawn_blocking(move || Ok(db.media.get(key)?.map(|m| m.to_vec()))).await?
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let custom = self.custom.clone();
        let key = key.to_owned();
        spawn_blocking(move || Ok(custom.get(key)?.map(|v| v.to_vec()))).await?
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let ret = self.custom.insert(key, value)?.map(|v| v.to_vec());
        self.inner.flush_async().await?;

        Ok(ret)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.media.remove(
            (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
                .encode(),
        )?;

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let keys = self.media.scan_prefix(uri.as_str().encode()).keys();

        let mut batch = sled::Batch::default();
        for key in keys {
            batch.remove(key?);
        }

        Ok(self.media.apply_batch(batch)?)
    }

    async fn get_timeline(
        &self,
        room_id: &RoomId,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: Option<usize>,
        direction: Direction,
    ) -> Result<Option<StoredTimelineSlice>> {
        let beginning = if let Some(start) = start {
            if let Some(batch) = self
                .event_id_to_position
                .get((room_id.as_str(), start.as_str()).encode())?
                .map(EventPosition::from)
            {
                batch
            } else {
                return Ok(None);
            }
        } else if let Some(token) = self.get_sync_token().await? {
            // The timeline beyond the sync token isn't known, so don't bother to check the
            // store.
            match direction {
                Direction::Forward => {
                    return Ok(Some(StoredTimelineSlice::new(Vec::new(), Some(token))));
                }
                Direction::Backward => {
                    if let Some(batch) = self
                        .start_token_to_batch_idx_position
                        .get((room_id.as_str(), token.as_str()).encode())?
                        .map(EventPosition::from)
                    {
                        batch
                    } else {
                        return Ok(Some(StoredTimelineSlice::new(Vec::new(), Some(token))));
                    }
                }
            }
        } else {
            return Ok(None);
        };

        let ending = if let Some(end) = end {
            self.event_id_to_position
                .get((room_id.as_str(), end.as_str()).encode())?
                .map(EventPosition::from)
        } else {
            None
        };

        let (mut batch_idx, mut position) = (beginning.batch_idx, beginning.position);
        let mut events: Vec<SyncRoomEvent> = Vec::new();
        let mut token: Option<String>;

        match direction {
            Direction::Forward => loop {
                token = None;

                let current_limit_position =
                    limit.map(|limit| position - (limit - events.len()) + 1);
                let current_limit = if let Some(end_position) = ending
                    .filter(|end_batch| end_batch.batch_idx == batch_idx)
                    .map(|end_batch| end_batch.position)
                {
                    if let Some(current_limit_position) = current_limit_position {
                        if current_limit_position > end_position {
                            Some(current_limit_position)
                        } else {
                            Some(end_position)
                        }
                    } else {
                        Some(end_position)
                    }
                } else {
                    current_limit_position
                };

                let range = if let Some(current_limit) = current_limit {
                    self.timeline_events.range(
                        (room_id.as_str(), batch_idx, current_limit).encode()
                            ..=(room_id.as_str(), batch_idx, position).encode(),
                    )
                } else {
                    self.timeline_events.range(
                        (room_id, batch_idx).encode()
                            ..=(room_id.as_str(), batch_idx, position).encode(),
                    )
                };

                let mut part: Vec<SyncRoomEvent> = range
                    .rev()
                    .filter_map(move |e| self.deserialize_event::<SyncRoomEvent>(&e.ok()?.1).ok())
                    .collect();

                events.append(&mut part);

                if let Some(limit) = limit {
                    if events.len() >= limit {
                        break;
                    }
                }

                if let Some(end) = end {
                    if let Some(last) = events.last() {
                        if &last.event_id().unwrap() == end {
                            break;
                        }
                    }
                }

                // Not enought events where found in this batch, therefore, go to the previous
                // batch.
                if let Some(start_token) = self
                    .batch_idx_to_start_token
                    .get((room_id, batch_idx).encode())?
                    .map(|t| decode_key_value(&t, 0).unwrap())
                {
                    if let Some(next_batch) = self
                        .end_token_to_batch_idx_position
                        .get((room_id.as_str(), start_token.as_str()).encode())?
                        .map(EventPosition::from)
                    {
                        // We don't have any other batch with this token
                        if batch_idx == next_batch.batch_idx {
                            token = Some(start_token);
                            break;
                        }

                        batch_idx = next_batch.batch_idx;
                        position = next_batch.position - 1;
                        continue;
                    }

                    token = Some(start_token);
                }
                break;
            },
            Direction::Backward => loop {
                token = None;

                let current_limit_position = limit.map(|limit| position + (limit - events.len()));
                let current_limit = if let Some(end_position) = ending
                    .filter(|end_batch| end_batch.batch_idx == batch_idx)
                    .map(|end_batch| end_batch.position)
                {
                    let end_position = end_position + 1;
                    if let Some(current_limit_position) = current_limit_position {
                        if current_limit_position < end_position {
                            Some(current_limit_position)
                        } else {
                            Some(end_position)
                        }
                    } else {
                        Some(end_position)
                    }
                } else {
                    current_limit_position
                };

                let range = if let Some(current_limit) = current_limit {
                    self.timeline_events.range(
                        (room_id.as_str(), batch_idx, position).encode()
                            ..(room_id.as_str(), batch_idx, current_limit).encode(),
                    )
                } else {
                    self.timeline_events.range(
                        (room_id.as_str(), batch_idx, position).encode()
                            ..(room_id, batch_idx.next()).encode(),
                    )
                };

                let mut part = range
                    .filter_map(move |e| self.deserialize_event::<SyncRoomEvent>(&e.ok()?.1).ok())
                    .collect();

                events.append(&mut part);

                if let Some(limit) = limit {
                    if events.len() >= limit {
                        break;
                    }
                }

                if let Some(end) = end {
                    if let Some(last) = events.last() {
                        if &last.event_id().unwrap() == end {
                            break;
                        }
                    }
                }

                // Not enought events where found in this batch, therefore, go to the previous
                // batch.
                if let Some(end_token) = self
                    .batch_idx_to_end_token
                    .get((room_id, batch_idx).encode())?
                    .map(|t| decode_key_value(&t, 0).unwrap())
                {
                    if let Some(prev_batch) = self
                        .start_token_to_batch_idx_position
                        .get((room_id.as_str(), end_token.as_str()).encode())?
                        .map(EventPosition::from)
                    {
                        // We don't have any other batch with this token
                        if batch_idx == prev_batch.batch_idx {
                            token = Some(end_token);
                            break;
                        }

                        batch_idx = prev_batch.batch_idx;
                        position = prev_batch.position;
                        continue;
                    }

                    token = Some(end_token);
                }
                break;
            },
        }

        Ok(Some(StoredTimelineSlice::new(events, token)))
    }

    async fn remove_timeline(&self, room_id: Option<&RoomId>) -> Result<()> {
        let forest = [
            &self.event_id_to_position,
            &self.timeline_events,
            &self.batch_idx_to_start_token,
            &self.batch_idx_to_end_token,
            &self.start_token_to_batch_idx_position,
            &self.end_token_to_batch_idx_position,
            &self.highest_batch_idx,
        ];

        for tree in forest {
            let mut batch = sled::Batch::default();
            let keys = if let Some(room_id) = room_id {
                tree.scan_prefix(room_id.as_str().encode()).keys()
            } else {
                tree.iter().keys()
            };

            for key in keys {
                batch.remove(key?);
            }

            tree.apply_batch(batch)?
        }

        Ok(())
    }
}

#[async_trait]
impl StateStore for SledStore {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.save_filter(filter_name, filter_id).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_filter(&self, filter_id: &str) -> Result<Option<String>> {
        self.get_filter(filter_id).await
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        self.get_sync_token().await
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_user_ids_stream(room_id).await?.try_collect().await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_invited_user_ids(room_id).await?.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_joined_user_ids(room_id).await?.try_collect().await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos().await?.try_collect().await
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_stripped_room_infos().await?.try_collect().await
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        self.get_users_with_display_name(room_id, display_name).await
    }

    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(EventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id).await
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_custom_value(key).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.add_media_content(request, data).await
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        self.get_media_content(request).await
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.remove_media_content(request).await
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        self.remove_media_content_for_uri(uri).await
    }

    async fn get_timeline(
        &self,
        room_id: &RoomId,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: Option<usize>,
        direction: Direction,
    ) -> Result<Option<StoredTimelineSlice>> {
        self.get_timeline(room_id, start, end, limit, direction).await
    }

    async fn remove_timeline(&self, room_id: Option<&RoomId>) -> Result<()> {
        self.remove_timeline(room_id).await
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use http::Response;
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{
        api::{
            client::r0::{
                media::get_content_thumbnail::Method,
                message::get_message_events::{Direction, Response as MessageResponse},
                sync::sync_events::Response as SyncResponse,
            },
            IncomingResponse,
        },
        event_id,
        events::{
            room::{
                member::{MembershipState, RoomMemberEventContent},
                power_levels::RoomPowerLevelsEventContent,
            },
            AnySyncStateEvent, EventType, Unsigned,
        },
        mxc_uri,
        receipt::ReceiptType,
        room_id,
        serde::Raw,
        uint, user_id, EventId, MilliSecondsSinceUnixEpoch, UserId,
    };
    use serde_json::json;

    use super::{Result, SledStore, StateChanges};
    use crate::{
        deserialized_responses::{MemberEvent, SyncRoomEvent, TimelineSlice},
        media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
        StateStore,
    };

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn power_level_event() -> Raw<AnySyncStateEvent> {
        let content = RoomPowerLevelsEventContent::default();

        let event = json!({
            "event_id": EventId::try_from("$h29iv0s8:example.com").unwrap(),
            "content": content,
            "sender": user_id(),
            "type": "m.room.power_levels",
            "origin_server_ts": 0u64,
            "state_key": "",
            "unsigned": Unsigned::default(),
        });

        serde_json::from_value(event).unwrap()
    }

    fn membership_event() -> MemberEvent {
        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content: RoomMemberEventContent::new(MembershipState::Join),
            sender: user_id(),
            origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = SledStore::open().unwrap();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store.get_member_event(&room_id, &user_id).await.unwrap().is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await.unwrap();
        assert!(store.get_member_event(&room_id, &user_id).await.unwrap().is_some());

        let members = store.get_user_ids(&room_id).await.unwrap();
        assert!(!members.is_empty())
    }

    #[async_test]
    async fn test_power_level_saving() {
        let store = SledStore::open().unwrap();
        let room_id = room_id!("!test:localhost");

        let raw_event = power_level_event();
        let event = raw_event.deserialize().unwrap();

        assert!(store
            .get_state_event(&room_id, EventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes.add_state_event(&room_id, event, raw_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_state_event(&room_id, EventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_some());
    }

    #[async_test]
    async fn test_receipts_saving() {
        let store = SledStore::open().unwrap();

        let room_id = room_id!("!test:localhost");

        let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org");
        let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org");

        let first_receipt_event = serde_json::from_value(json!({
            first_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451550453u64
                    }
                }
            }
        }))
        .unwrap();

        let second_receipt_event = serde_json::from_value(json!({
            second_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451551453u64
                    }
                }
            }
        }))
        .unwrap();

        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, first_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some(),);
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, second_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[async_test]
    async fn test_media_content() {
        let store = SledStore::open().unwrap();

        let uri = mxc_uri!("mxc://localhost/media");
        let content: Vec<u8> = "somebinarydata".into();

        let request_file =
            MediaRequest { media_type: MediaType::Uri(uri.clone()), format: MediaFormat::File };

        let request_thumbnail = MediaRequest {
            media_type: MediaType::Uri(uri.clone()),
            format: MediaFormat::Thumbnail(MediaThumbnailSize {
                method: Method::Crop,
                width: uint!(100),
                height: uint!(100),
            }),
        };

        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.remove_media_content(&request_file).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.add_media_content(&request_thumbnail, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_some());

        store.remove_media_content_for_uri(&uri).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());
    }

    #[async_test]
    async fn test_custom_storage() -> Result<()> {
        let key = "my_key";
        let value = &[0, 1, 2, 3];
        let store = SledStore::open()?;

        store.set_custom_value(key.as_bytes(), value.to_vec()).await?;

        let read = store.get_custom_value(key.as_bytes()).await?;

        assert_eq!(Some(value.as_ref()), read.as_deref());

        Ok(())
    }

    #[async_test]
    async fn test_timeline() {
        let store = SledStore::open().unwrap();
        let mut stored_events = Vec::new();
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        assert!(store
            .get_timeline(&room_id, None, None, None, Direction::Forward)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .is_none());

        // Add a sync response
        let sync = SyncResponse::try_from_http_response(
            Response::builder().body(serde_json::to_vec(&*test_json::MORE_SYNC).unwrap()).unwrap(),
        )
        .unwrap();

        let timeline = &sync.rooms.join[&room_id].timeline;
        let events: Vec<SyncRoomEvent> =
            timeline.events.iter().rev().cloned().map(Into::into).collect();

        stored_events.append(&mut events.clone());

        let timeline_slice =
            TimelineSlice::new(events, sync.next_batch.clone(), timeline.prev_batch.clone());
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        assert!(store
            .get_timeline(&room_id, None, None, None, Direction::Forward)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .is_none());

        // Add the next batch token
        let changes =
            StateChanges { sync_token: Some(sync.next_batch.clone()), ..Default::default() };
        store.save_changes(&changes).await.unwrap();

        let forward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Forward)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forward_events.events.len(), 0);
        assert_eq!(forward_events.token, Some(sync.next_batch.clone()));

        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backward_events.events.len(), 6);
        assert_eq!(backward_events.token, timeline.prev_batch.clone());

        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Add a message batch before the sync response
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::SYNC_ROOM_MESSAGES_BATCH_1).unwrap())
                .unwrap(),
        )
        .unwrap();

        let events: Vec<SyncRoomEvent> = messages.chunk.iter().cloned().map(Into::into).collect();

        stored_events.append(&mut events.clone());

        let timeline_slice = TimelineSlice::new(
            events.clone(),
            messages.start.clone().unwrap(),
            messages.end.clone(),
        );
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        // Add the same message batch again
        let timeline_slice =
            TimelineSlice::new(events, messages.start.clone().unwrap(), messages.end.clone());
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), 9);
        assert_eq!(backward_events.token, messages.end.clone());

        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Add a batch after the previous batch
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::SYNC_ROOM_MESSAGES_BATCH_2).unwrap())
                .unwrap(),
        )
        .unwrap();

        let events: Vec<SyncRoomEvent> = messages.chunk.iter().cloned().map(Into::into).collect();

        stored_events.append(&mut events.clone());

        let timeline_slice =
            TimelineSlice::new(events, messages.start.clone().unwrap(), messages.end.clone());
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backward_events.events.len(), 12);
        assert_eq!(backward_events.token, messages.end.clone());

        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let first_block_end = messages.end.clone();
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::GAPPED_ROOM_MESSAGES_BATCH_1).unwrap())
                .unwrap(),
        )
        .unwrap();

        let gapped_events: Vec<SyncRoomEvent> =
            messages.chunk.iter().cloned().map(Into::into).collect();

        // Add a detached batch to create a gap in the known timeline
        let timeline_slice = TimelineSlice::new(
            gapped_events.clone(),
            messages.start.clone().unwrap(),
            messages.end.clone(),
        );
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();
        let gapped_block_end = messages.end.clone();
        assert_eq!(backward_events.events.len(), 12);
        assert_eq!(backward_events.token, first_block_end);

        // Fill the gap that was created before
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::GAPPED_ROOM_MESSAGES_FILLER).unwrap())
                .unwrap(),
        )
        .unwrap();

        let events: Vec<SyncRoomEvent> = messages.chunk.iter().cloned().map(Into::into).collect();

        stored_events.append(&mut events.clone());
        stored_events.append(&mut gapped_events.clone());

        let timeline_slice = TimelineSlice::new(
            events.clone(),
            messages.start.clone().unwrap(),
            messages.end.clone(),
        );
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        // Read all of the known timeline from the beginning backwards
        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, gapped_block_end);
        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // The most recent event
        let first_event = event_id!("$098237280074GZeOm:localhost");
        let backward_events = store
            .get_timeline(&room_id, Some(&first_event), None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, gapped_block_end);
        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let last_event = event_id!("$1444812213350496Ccccr:example.com");

        // Read from the last known event forward
        let forward_events = store
            .get_timeline(&room_id, Some(&last_event), None, None, Direction::Forward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(forward_events.events.len(), stored_events.len());
        assert_eq!(forward_events.token, Some(sync.next_batch.clone()));
        assert!(forward_events
            .events
            .iter()
            .rev()
            .zip(backward_events.events)
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Read from the last known event backwards
        let events = store
            .get_timeline(&room_id, Some(&last_event), None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events.events.len(), 1);
        assert_eq!(events.events.first().unwrap().event_id().unwrap(), last_event);
        assert_eq!(events.token, gapped_block_end);

        let events = store
            .get_timeline(&room_id, Some(&last_event), None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events.events.len(), 1);
        assert_eq!(events.events.first().unwrap().event_id().unwrap(), last_event);
        assert_eq!(events.token, gapped_block_end);

        let end_event = event_id!("$1444812213350496Caaar:example.com");

        // Get a slice of the timeline
        let backward_events = store
            .get_timeline(&room_id, Some(&first_event), Some(&end_event), None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        let expected_events = &stored_events[..stored_events.len() - 2];

        assert_eq!(backward_events.events.len(), expected_events.len());
        assert_eq!(backward_events.token, None);
        assert!(backward_events
            .events
            .iter()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let forward_events = store
            .get_timeline(&room_id, Some(&end_event), Some(&first_event), None, Direction::Forward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(forward_events.events.len(), expected_events.len());
        assert_eq!(forward_events.token, None);

        assert!(forward_events
            .events
            .iter()
            .rev()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Get a slice of the timeline where the end isn't known
        let unknown_end_event = event_id!("$XXXXXXXXX:example.com");

        let backward_events = store
            .get_timeline(
                &room_id,
                Some(&first_event),
                Some(&unknown_end_event),
                None,
                Direction::Backward,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, gapped_block_end);
        assert!(backward_events
            .events
            .iter()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let forward_events = store
            .get_timeline(
                &room_id,
                Some(&first_event),
                Some(&unknown_end_event),
                None,
                Direction::Forward,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(forward_events.events.len(), 1);
        assert_eq!(forward_events.events.first().unwrap().event_id().unwrap(), first_event);
        assert_eq!(forward_events.token, Some(sync.next_batch.clone()));

        // Get a slice of the timeline with limit with more then the number of events
        // between start and end
        let limit = 4;
        let backward_events = store
            .get_timeline(
                &room_id,
                Some(&first_event),
                Some(&end_event),
                Some(limit),
                Direction::Backward,
            )
            .await
            .unwrap()
            .unwrap();

        let expected_events = &stored_events[..limit];
        assert_eq!(backward_events.events.len(), limit);
        assert_eq!(backward_events.token, None);
        assert!(backward_events
            .events
            .iter()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let forward_events = store
            .get_timeline(
                &room_id,
                Some(&last_event),
                Some(&first_event),
                Some(limit),
                Direction::Forward,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(forward_events.events.len(), limit);
        assert_eq!(forward_events.token, None);

        let expected_events = &stored_events[stored_events.len() - limit..];
        assert!(forward_events
            .events
            .iter()
            .rev()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Get a slice of the timeline with limit with less then limit events between
        // start and end
        let limit = 30;
        let backward_events = store
            .get_timeline(
                &room_id,
                Some(&first_event),
                Some(&last_event),
                Some(limit),
                Direction::Backward,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, None);
        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        let forward_events = store
            .get_timeline(
                &room_id,
                Some(&last_event),
                Some(&first_event),
                Some(limit),
                Direction::Forward,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(forward_events.events.len(), stored_events.len());
        assert_eq!(forward_events.token, None);

        assert!(forward_events
            .events
            .iter()
            .rev()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Add a second sync response
        let sync = SyncResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::MORE_SYNC_2).unwrap())
                .unwrap(),
        )
        .unwrap();

        let timeline = &sync.rooms.join[&room_id].timeline;
        let events: Vec<SyncRoomEvent> =
            timeline.events.iter().rev().cloned().map(Into::into).collect();

        for event in events.clone().into_iter().rev() {
            stored_events.insert(0, event);
        }

        let timeline_slice =
            TimelineSlice::new(events, sync.next_batch.clone(), timeline.prev_batch.clone());
        let mut changes =
            StateChanges { sync_token: Some(sync.next_batch.clone()), ..Default::default() };
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        // Read all of the known timeline from the beginning backwards
        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, gapped_block_end);
        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Add an overlapping timeline slice to the store
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::OVERLAPPING_ROOM_MESSAGES_BATCH_1).unwrap())
                .unwrap(),
        )
        .unwrap();

        let events: Vec<SyncRoomEvent> = messages.chunk.iter().cloned().map(Into::into).collect();

        stored_events.push(events.last().cloned().unwrap());

        let timeline_slice = TimelineSlice::new(
            events.clone(),
            messages.start.clone().unwrap(),
            messages.end.clone(),
        );
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        // Read all of the known timeline from the beginning backwards
        let backward_events = store
            .get_timeline(&room_id, None, None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        let end_token = messages.end;
        assert_eq!(backward_events.events.len(), stored_events.len());
        assert_eq!(backward_events.token, end_token);
        assert!(backward_events
            .events
            .iter()
            .zip(stored_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Add an overlapping patch to the start of everything
        let messages = MessageResponse::try_from_http_response(
            Response::builder()
                .body(serde_json::to_vec(&*test_json::OVERLAPPING_ROOM_MESSAGES_BATCH_2).unwrap())
                .unwrap(),
        )
        .unwrap();

        let events: Vec<SyncRoomEvent> = messages.chunk.iter().cloned().map(Into::into).collect();

        let mut expected_events = events.clone();
        expected_events.extend_from_slice(&stored_events[1..]);

        let timeline_slice = TimelineSlice::new(
            events.clone(),
            messages.start.clone().unwrap(),
            messages.end.clone(),
        );
        let mut changes = StateChanges::default();
        changes.add_timeline(&room_id, timeline_slice);
        store.save_changes(&changes).await.unwrap();

        let start_event = event_id!("$1444812213350496Cbbbr3:example.com");
        // Read all of the known timeline from the beginning backwards
        let backward_events = store
            .get_timeline(&room_id, Some(&start_event), None, None, Direction::Backward)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(backward_events.events.len(), expected_events.len());
        assert_eq!(backward_events.token, end_token);
        assert!(backward_events
            .events
            .iter()
            .zip(expected_events.iter())
            .all(|(a, b)| a.event_id() == b.event_id()));

        // Clear the timeline for a room
        assert!(store.remove_timeline(Some(&room_id)).await.is_ok());

        assert_eq!(
            store
                .get_timeline(&room_id, None, None, None, Direction::Forward)
                .await
                .unwrap()
                .unwrap()
                .token
                .unwrap(),
            sync.next_batch
        );
        assert_eq!(
            store
                .get_timeline(&room_id, None, None, None, Direction::Backward)
                .await
                .unwrap()
                .unwrap()
                .token
                .unwrap(),
            sync.next_batch
        );
    }
}
