mod encoding;
mod offhand;
mod prediction;

use acacia_client::proto::packets::ItemStackResponse;
use acacia_client::proto::types::{
    BlockCoordinates, ContainerSlotType, FullContainerName, ItemStackResponsesItem, ItemStackResponsesItemContainersItem,
    ItemStackResponsesItemContainersItemSlotsItem, ItemStackResponsesItemStatus, WindowType,
};
use acacia_client::proto::{encode_packet, Packet, RawPacket};
use bytes::BytesMut;

use super::plan::Plan;
use super::slot::{BlockKind, Screen};
use super::{Op, SlotRef};
use crate::state::{Container, GameState, ItemStack};

pub(super) fn raw<T: Packet>(packet: &T) -> RawPacket {
    let mut buf = BytesMut::new();
    encode_packet(packet, &mut buf);
    RawPacket::parse(buf.freeze()).unwrap()
}

pub(super) fn stack(network_id: i32, count: u16, stack_id: i32) -> ItemStack {
    ItemStack { network_id, count, stack_network_id: Some(stack_id), ..ItemStack::default() }
}

/// A state whose main inventory holds `items` as `(slot, stack)`.
pub(super) fn state_with(items: &[(usize, ItemStack)]) -> GameState {
    let mut state = GameState::default();
    for (slot, item) in items {
        state.inventory.main[*slot] = item.clone();
    }
    state
}

pub(super) fn open_container(state: &mut GameState, window_type: WindowType, slots: Vec<ItemStack>) {
    state.containers.open = Some(Container {
        window_id: 1,
        window_type,
        position: Some(BlockCoordinates { x: 0, y: 64, z: 0 }),
        entity: None,
        title: None,
        slots,
    });
}

pub(super) fn screen(state: &GameState) -> Screen {
    Screen::of(state, BlockKind::Other)
}

/// One `ItemStackResponse` entry; `slots` are `(container, slot, count, stack id)`.
pub(super) fn response(request_id: i32, ok: bool, slots: &[(ContainerSlotType, u8, u8, i32)]) -> ItemStackResponsesItem {
    let containers = slots
        .iter()
        .map(|&(kind, slot, count, stack_id)| ItemStackResponsesItemContainersItem {
            slot_type: FullContainerName { container_id: kind, dynamic_container_id: None },
            slots: vec![ItemStackResponsesItemContainersItemSlotsItem {
                slot,
                hotbar_slot: slot,
                count,
                item_stack_id: (count > 0).then_some(stack_id),
                custom_name: String::new(),
                filtered_custom_name: String::new(),
                durability_correction: 0,
            }],
        })
        .collect();
    let status = if ok { ItemStackResponsesItemStatus::Ok } else { ItemStackResponsesItemStatus::Error };
    ItemStackResponsesItem { status, request_id, containers: ok.then_some(containers) }
}

pub(super) fn response_packet(entries: Vec<ItemStackResponsesItem>) -> RawPacket {
    raw(&ItemStackResponse { responses: entries })
}

/// `Plan::applied`: the read-back `Bot::send_plan` uses when a write's response did not arrive. The
/// server's own slot updates are enough to call the request done (`items/actions.rs`).
#[test]
fn a_plan_applies_once_the_tracked_slots_hold_its_prediction() {
    let mut state = state_with(&[(0, stack(5, 3, 7))]);
    let plan = Plan::build(&state, screen(&state), &[Op::Transfer { from: SlotRef::Main(0), to: SlotRef::Main(9), count: 3 }]).unwrap();
    assert!(!plan.applied(&state), "nothing has moved yet");

    state.inventory.main[0] = ItemStack::default();
    state.inventory.main[9] = stack(5, 2, 9);
    assert!(!plan.applied(&state), "a partial move is not the prediction");

    state.inventory.main[9] = stack(5, 3, 9);
    assert!(plan.applied(&state), "counts match the prediction; the stack id is the server's");
}

#[test]
fn a_plan_that_moves_into_a_container_reads_the_open_window_back() {
    let mut state = state_with(&[(0, stack(5, 3, 7))]);
    open_container(&mut state, WindowType::Container, vec![ItemStack::default()]);
    let plan = Plan::build(&state, screen(&state), &[Op::Transfer { from: SlotRef::Main(0), to: SlotRef::Container(0), count: 3 }]).unwrap();
    assert!(!plan.applied(&state));

    state.inventory.main[0] = ItemStack::default();
    state.containers.open.as_mut().unwrap().slots[0] = stack(5, 3, 9);
    assert!(plan.applied(&state));

    // Closing the window hides the slots: nothing can be read back.
    state.containers.open = None;
    assert!(!plan.applied(&state));
}
