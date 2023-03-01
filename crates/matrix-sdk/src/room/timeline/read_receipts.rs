// Copyright 2023 Kévin Commaille
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

use std::{collections::HashMap, sync::Arc};

use eyeball_im::ObservableVector;
use indexmap::IndexMap;
use ruma::{
    events::receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
    EventId, OwnedEventId, OwnedUserId, UserId,
};
use tracing::error;

use super::{
    inner::{RoomDataProvider, TimelineInnerState},
    rfind_event_by_id, EventTimelineItem, TimelineItem,
};

struct FullReceipt<'a> {
    event_id: &'a EventId,
    user_id: &'a UserId,
    receipt_type: ReceiptType,
    receipt: &'a Receipt,
}

pub(super) fn handle_explicit_read_receipts(
    receipt_event_content: ReceiptEventContent,
    own_user_id: &UserId,
    timeline_state: &mut TimelineInnerState,
) {
    for (event_id, receipt_types) in receipt_event_content.0 {
        for (receipt_type, receipts) in receipt_types {
            // We only care about read receipts here.
            if !matches!(receipt_type, ReceiptType::Read | ReceiptType::ReadPrivate) {
                continue;
            }

            for (user_id, receipt) in receipts {
                if receipt.thread != ReceiptThread::Unthreaded {
                    continue;
                }

                let receipt_item_pos =
                    rfind_event_by_id(&timeline_state.items, &event_id).map(|(pos, _)| pos);
                let is_own_user_id = user_id == own_user_id;
                let full_receipt = FullReceipt {
                    event_id: &event_id,
                    user_id: &user_id,
                    receipt_type: receipt_type.clone(),
                    receipt: &receipt,
                };

                let read_receipt_updated = maybe_update_read_receipt(
                    full_receipt,
                    receipt_item_pos,
                    is_own_user_id,
                    &mut timeline_state.items,
                    &mut timeline_state.users_read_receipts,
                );

                if read_receipt_updated && !is_own_user_id {
                    // Update the new item pointed to by the user's read receipt.
                    let new_receipt_event_item = receipt_item_pos.and_then(|pos| {
                        let e = timeline_state.items[pos].as_event()?.as_remote()?;
                        Some((pos, e.clone()))
                    });

                    if let Some((pos, mut remote_event_item)) = new_receipt_event_item {
                        remote_event_item.read_receipts.insert(user_id, receipt);
                        timeline_state
                            .items
                            .set(pos, Arc::new(TimelineItem::Event(remote_event_item.into())));
                    }
                }
            }
        }
    }
}

/// Add an implicit read receipt to the given event item, if it is more recent
/// than the current read receipt for the sender of the event.
///
/// According to the spec, read receipts should not point to events sent by our
/// own user, but these events are used to reset the notification count, so we
/// need to handle them locally too. For that we create an "implicit" read
/// receipt, compared to the "explicit" ones sent by the client.
pub(super) fn maybe_add_implicit_read_receipt(
    item_pos: usize,
    event_item: &mut EventTimelineItem,
    is_own_event: bool,
    timeline_items: &mut ObservableVector<Arc<TimelineItem>>,
    users_read_receipts: &mut HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
) {
    let EventTimelineItem::Remote(remote_event_item) = event_item else {
        return;
    };

    let receipt = Receipt::new(remote_event_item.timestamp);
    let new_receipt = FullReceipt {
        event_id: &remote_event_item.event_id,
        user_id: &remote_event_item.sender.clone(),
        receipt_type: ReceiptType::Read,
        receipt: &receipt,
    };

    let read_receipt_updated = maybe_update_read_receipt(
        new_receipt,
        Some(item_pos),
        is_own_event,
        timeline_items,
        users_read_receipts,
    );
    if read_receipt_updated && !is_own_event {
        remote_event_item.read_receipts.insert(remote_event_item.sender.clone(), receipt);
    }
}

/// Update the timeline items with the given read receipt if it is more recent
/// than the current one.
///
/// In the process, this method removes the corresponding receipt from its old
/// item, if applicable, and updates the `users_read_receipts` map to use the
/// new receipt.
///
/// Returns true if the read receipt was saved.
///
/// Currently this method only works reliably if the timeline was started from
/// the end of the timeline.
fn maybe_update_read_receipt(
    receipt: FullReceipt<'_>,
    new_item_pos: Option<usize>,
    is_own_user_id: bool,
    timeline_items: &mut ObservableVector<Arc<TimelineItem>>,
    users_read_receipts: &mut HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
) -> bool {
    let old_event_id = users_read_receipts
        .get(receipt.user_id)
        .and_then(|receipts| receipts.get(&receipt.receipt_type))
        .map(|(event_id, _)| event_id);
    if old_event_id.map_or(false, |id| id == receipt.event_id) {
        // Nothing to do.
        return false;
    }

    let old_item = old_event_id.and_then(|e| {
        let (pos, item) = rfind_event_by_id(timeline_items, e)?;
        Some((pos, item.as_remote()?))
    });

    if let Some((old_receipt_pos, old_event_item)) = old_item {
        let Some(new_receipt_pos) = new_item_pos else {
            // The old receipt is likely more recent since we can't find the event of the
            // new receipt in the timeline. Even if it isn't, we wouldn't know where to put
            // it.
            return false;
        };

        if old_receipt_pos > new_receipt_pos {
            // The old receipt is more recent than the new one.
            return false;
        }

        if !is_own_user_id {
            // Remove the read receipt for this user from the old event.
            let mut old_event_item = old_event_item.clone();
            if old_event_item.read_receipts.remove(receipt.user_id).is_none() {
                error!("inconsistent state: old event item for user's read receipt doesn't have a receipt for the user");
            }
            timeline_items
                .set(old_receipt_pos, Arc::new(TimelineItem::Event(old_event_item.into())));
        }
    }

    // The new receipt is deemed more recent from now on because:
    // - If old_receipt_item is Some, we already checked all the cases where it
    //   wouldn't be more recent.
    // - If both old_receipt_item and new_receipt_item are None, they are both
    //   explicit read receipts so the server should only send us a more recent
    //   receipt.
    // - If old_receipt_item is None and new_receipt_item is Some, the new receipt
    //   is likely more recent because it has a place in the timeline.
    users_read_receipts
        .entry(receipt.user_id.to_owned())
        .or_default()
        .insert(receipt.receipt_type, (receipt.event_id.to_owned(), receipt.receipt.clone()));

    true
}

/// Load the read receipts from the store for the given event ID.
pub(super) async fn load_read_receipts_for_event<P: RoomDataProvider>(
    event_id: &EventId,
    timeline_state: &mut TimelineInnerState,
    room_data_provider: &P,
) -> IndexMap<OwnedUserId, Receipt> {
    let read_receipts = room_data_provider.read_receipts_for_event(event_id).await;

    // Filter out receipts for our own user.
    let own_user_id = room_data_provider.own_user_id();
    let read_receipts: IndexMap<OwnedUserId, Receipt> =
        read_receipts.into_iter().filter(|(user_id, _)| user_id != own_user_id).collect();

    // Keep track of the user's read receipt.
    for (user_id, receipt) in read_receipts.clone() {
        // Only insert the read receipt if the user is not known to avoid conflicts with
        // `TimelineInner::handle_read_receipts`.
        if !timeline_state.users_read_receipts.contains_key(&user_id) {
            timeline_state
                .users_read_receipts
                .entry(user_id)
                .or_default()
                .insert(ReceiptType::Read, (event_id.to_owned(), receipt));
        }
    }

    read_receipts
}