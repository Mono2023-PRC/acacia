//! t138: the offhand's second shape — a swap refused with `50 FailedToValidateDstSlot` becomes a
//! `Take` onto the cursor and a `Place` into the offhand, and the request says exactly what it sent.

use super::*;
use crate::items::plan::Plan;
use crate::items::request::describe_request;
use crate::items::slot::OffhandAddress;

fn cursor_screen(state: &GameState) -> Screen {
    screen(state).with_offhand(OffhandAddress::ViaCursor)
}

fn kinds(plan: &Plan) -> Vec<String> {
    let packet = plan.request(-3);
    packet.requests.iter().flat_map(|r| r.actions.iter()).map(|a| format!("{:?}", a.type_id)).collect()
}

#[test]
fn a_refused_exchange_becomes_a_take_and_a_place() {
    let state = state_with(&[(0, stack(1, 5, 11))]);
    let plan = Plan::build(&state, cursor_screen(&state), &[Op::Swap { a: SlotRef::Main(0), b: SlotRef::Offhand }]).unwrap();
    let kinds = kinds(&plan);
    assert_eq!(kinds.len(), 2, "two clicks, not one exchange: {kinds:?}");
    assert!(kinds[0].contains("Take"), "the stack goes to the cursor first: {kinds:?}");
    assert!(kinds[1].contains("Place"), "then onto the offhand: {kinds:?}");
    // The prediction must end with the stack in the offhand and nothing left on the cursor.
    assert_eq!(plan.predicted(SlotRef::Offhand).map(|s| s.count), Some(5));
    assert!(plan.predicted(SlotRef::Cursor).map(|s| s.is_empty()).unwrap_or(true));
}

#[test]
fn an_occupied_offhand_is_not_forced_through_the_cursor() {
    let mut state = state_with(&[(0, stack(1, 5, 11))]);
    state.inventory.offhand = stack(2, 1, 12);
    let err = Plan::build(&state, cursor_screen(&state), &[Op::Swap { a: SlotRef::Main(0), b: SlotRef::Offhand }])
        .expect_err("an exchange needs a third click, which this shape does not have");
    assert!(format!("{err:?}").contains("third click"), "{err:?}");
}

#[test]
fn the_request_says_the_whole_structure_it_sent() {
    let state = state_with(&[(0, stack(1, 5, 11))]);
    let plan = Plan::build(&state, cursor_screen(&state), &[Op::Swap { a: SlotRef::Main(0), b: SlotRef::Offhand }]).unwrap();
    let sent = describe_request(&plan.request(-3)).join("; ");
    // Container id, its protocol number, the slot index, the stack id and the dynamic id — the relay
    // echoes none of these back, so the message is the only record (t137: `ContainerInfo: null`).
    assert!(sent.contains("Offhand(34)"), "{sent}");
    assert!(sent.contains("slot=0"), "{sent}");
    assert!(sent.contains("stack="), "{sent}");
    assert!(sent.contains("dyn=none"), "{sent}");
}
