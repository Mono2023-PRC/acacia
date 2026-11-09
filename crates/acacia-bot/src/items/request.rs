use acacia_client::proto::packets::{ItemStackRequest as ItemStackRequestPacket, ItemStackResponse};
use acacia_client::proto::types::{
    ItemStackRequest, ItemStackRequestActionsItem as Action, ItemStackRequestActionsItemContent as Content,
    ItemStackRequestActionsItemContentBeaconPayment, ItemStackRequestActionsItemContentConsume, ItemStackRequestActionsItemContentCreate,
    ItemStackRequestActionsItemContentDestroy, ItemStackRequestActionsItemContentDrop,
    ItemStackRequestActionsItemContentPlace, ItemStackRequestActionsItemContentSwap,
    ItemStackRequestActionsItemContentTake, ItemStackRequestActionsItemTypeId as TypeId, ItemStackRequestCause,
    ItemStackResponsesItem, ItemStackResponsesItemStatus, StackRequestSlotInfo,
};
use acacia_client::proto::{Packet, RawPacket};

use super::slot::{describe_slot, names_request_stack};
use super::{ui, SlotRef};
use crate::{ActionError, Bot};

/// One step of an item stack request, in terms of the bot's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Move `count` items onto an empty or matching stack (sent as `Take` into the cursor, `Place` otherwise).
    Transfer { from: SlotRef, to: SlotRef, count: u8 },
    Swap { a: SlotRef, b: SlotRef },
    /// Throw `count` items into the world.
    Drop { from: SlotRef, count: u8 },
    /// Delete `count` items (creative mode only).
    Destroy { from: SlotRef, count: u8 },
    /// Use up `count` crafting inputs; only valid after a craft action in the same request.
    Consume { from: SlotRef, count: u8 },
    /// Put result `index` of a multi-output craft into the created-output slot.
    Create { index: u8 },
}

/// Request ids of one session: vanilla's first request is -3, then -5, -7, ... (2026-10-02 capture);
/// even negative ids belong to legacy InventoryTransactions.
#[derive(Debug)]
pub(crate) struct RequestIds {
    pub(super) next: i32,
    /// The status of the last refused request, for callers that answer a refusal with another shape.
    pub(super) last_rejection: Option<Rejection>,
    /// The slot descriptors of the last request sent (`describe_request`), quoted in a refusal.
    pub(super) last_request: Vec<String>,
}

impl Default for RequestIds {
    fn default() -> Self {
        RequestIds { next: -3, last_rejection: None, last_request: Vec::new() }
    }
}

impl RequestIds {
    pub fn next(&mut self) -> i32 {
        let id = self.next;
        // Wraps after ~1e9 requests; restart the sequence.
        self.next = id.checked_sub(2).unwrap_or(-3);
        id
    }
}

/// The wire action for `op`; `info` gives each slot's request view as it is before the op runs,
/// told whether the op is an automatic move (see [`auto_move`]).
pub(crate) fn encode(op: Op, info: impl Fn(SlotRef, bool) -> StackRequestSlotInfo) -> Action {
    let auto = auto_move(op);
    let info = |slot| info(slot, auto);
    let (type_id, content) = match op {
        Op::Transfer { from, to: SlotRef::Cursor, count } => {
            let (source, destination) = (info(from), info(SlotRef::Cursor));
            (TypeId::Take, Content::Take(ItemStackRequestActionsItemContentTake { count, source, destination }))
        }
        Op::Transfer { from, to, count } => {
            let (source, destination) = (info(from), info(to));
            (TypeId::Place, Content::Place(ItemStackRequestActionsItemContentPlace { count, source, destination }))
        }
        Op::Swap { a, b } => {
            (TypeId::Swap, Content::Swap(ItemStackRequestActionsItemContentSwap { source: info(a), destination: info(b) }))
        }
        Op::Drop { from, count } => {
            (TypeId::Drop, Content::Drop(ItemStackRequestActionsItemContentDrop { count, source: info(from), randomly: false }))
        }
        Op::Destroy { from, count } => {
            (TypeId::Destroy, Content::Destroy(ItemStackRequestActionsItemContentDestroy { count, source: info(from) }))
        }
        Op::Consume { from, count } => {
            (TypeId::Consume, Content::Consume(ItemStackRequestActionsItemContentConsume { count, source: info(from) }))
        }
        Op::Create { index } => (TypeId::Create, Content::Create(ItemStackRequestActionsItemContentCreate { result_slot_id: index })),
    };
    action(type_id, content)
}

/// Moves the client makes for the player rather than slot to slot: consuming craft inputs, moving
/// out of a container or workstation, and filling a crafting grid or trade payment slot. Vanilla
/// names the player's slots in these `HotbarAndInventory`, otherwise `Hotbar`/`Inventory`
/// (2026-10-02 capture; the own inventory screen per BDS, see `SlotRef::wire`).
fn auto_move(op: Op) -> bool {
    match op {
        Op::Consume { .. } => true,
        Op::Transfer { from, to, .. } => {
            !from.is_player() || matches!(to, SlotRef::Ui(ui::TRADE_INGREDIENT_1 | ui::TRADE_INGREDIENT_2 | ui::CRAFTING_2X2..=ui::CRAFTING_3X3_LAST))
        }
        _ => false,
    }
}

/// `BeaconPayment` with Bedrock effect ids (0 = none).
pub(crate) fn beacon_payment(primary_effect: i32, secondary_effect: i32) -> Action {
    action(TypeId::BeaconPayment, Content::BeaconPayment(ItemStackRequestActionsItemContentBeaconPayment { primary_effect, secondary_effect }))
}

pub(crate) fn action(type_id: TypeId, content: Content) -> Action {
    Action { type_id, legacy_type_id: legacy_type_id(type_id), content }
}

/// The legacy id still counts the removed `PlaceInContainer`/`TakeOutContainer` (7, 8), so every
/// later action's legacy id is its variant + 2 (gophertunnel `item_stack.go`).
fn legacy_type_id(type_id: TypeId) -> u8 {
    let variant = type_id.to_raw();
    (if variant >= 7 { variant + 2 } else { variant }) as u8
}

/// The request body besides its actions; only anvil and cartography renames carry text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Texts {
    pub custom_names: Vec<String>,
    pub cause: ItemStackRequestCause,
}

impl Default for Texts {
    fn default() -> Self {
        // Vanilla sends -1 ("none") with every request that carries no text.
        Texts { custom_names: Vec::new(), cause: ItemStackRequestCause::Unknown(-1) }
    }
}

pub(crate) fn request_packet(request_id: i32, actions: Vec<Action>, texts: Texts) -> ItemStackRequestPacket {
    ItemStackRequestPacket {
        requests: vec![ItemStackRequest { request_id, actions, custom_names: texts.custom_names, cause: texts.cause }],
    }
}

/// The response to request `request_id`, if `packet` carries it.
pub(crate) fn response_for(packet: &RawPacket, request_id: i32) -> Option<ItemStackResponsesItem> {
    if packet.id != ItemStackResponse::ID {
        return None;
    }
    let response: ItemStackResponse = packet.decode().ok()?;
    response.responses.into_iter().find(|r| r.request_id == request_id)
}

/// The name a response status carries in the protocol table.
///
/// The names are the vendored gophertunnel fork's (`mousetunnel/minecraft/protocol/item_stack.go`,
/// `ItemStackResponseStatusOK = iota` .. `ItemStackResponseStatusScreenStackError`), which is the
/// same enum the server answers with; nothing here is guessed from the number alone.
pub(crate) fn status_name(code: u8) -> &'static str {
    match code {
        0 => "OK",
        1 => "Error",
        2 => "InvalidRequestActionType",
        3 => "ActionRequestNotAllowed",
        4 => "ScreenHandlerEndRequestFailed",
        5 => "ItemRequestActionHandlerCommitFailed",
        6 => "InvalidRequestCraftActionType",
        7 => "InvalidCraftRequest",
        8 => "InvalidCraftRequestScreen",
        9 => "InvalidCraftResult",
        10 => "InvalidCraftResultIndex",
        11 => "InvalidCraftResultItem",
        12 => "InvalidItemNetId",
        13 => "MissingCreatedOutputContainer",
        14 => "FailedToSetCreatedItemOutputSlot",
        15 => "RequestAlreadyInProgress",
        16 => "FailedToInitSparseContainer",
        17 => "ResultTransferFailed",
        18 => "ExpectedItemSlotNotFullyConsumed",
        19 => "ExpectedAnywhereItemNotFullyConsumed",
        20 => "ItemAlreadyConsumedFromSlot",
        21 => "ConsumedTooMuchFromSlot",
        22 => "MismatchSlotExpectedConsumedItem",
        23 => "MismatchSlotExpectedConsumedItemNetIdVariant",
        24 => "FailedToMatchExpectedSlotConsumedItem",
        25 => "FailedToMatchExpectedAllowedAnywhereConsumedItem",
        26 => "ConsumedItemOutOfAllowedSlotRange",
        27 => "ConsumedItemNotAllowed",
        28 => "PlayerNotInCreativeMode",
        29 => "InvalidExperimentalRecipeRequest",
        30 => "FailedToCraftCreative",
        31 => "FailedToGetLevelRecipe",
        32 => "FailedToFindRecipeByNetId",
        33 => "MismatchedCraftingSize",
        34 => "MissingInputSparseContainer",
        35 => "MismatchedRecipeForInputGridItems",
        36 => "EmptyCraftResults",
        37 => "FailedToEnchant",
        38 => "MissingInputItem",
        39 => "InsufficientPlayerLevelToEnchant",
        40 => "MissingMaterialItem",
        41 => "MissingActor",
        42 => "UnknownPrimaryEffect",
        43 => "PrimaryEffectOutOfRange",
        44 => "PrimaryEffectUnavailable",
        45 => "SecondaryEffectOutOfRange",
        46 => "SecondaryEffectUnavailable",
        47 => "DstContainerEqualToCreatedOutputContainer",
        48 => "DstContainerAndSlotEqualToSrcContainerAndSlot",
        49 => "FailedToValidateSrcSlot",
        50 => "FailedToValidateDstSlot",
        51 => "InvalidAdjustedAmount",
        52 => "InvalidItemSetType",
        53 => "InvalidTransferAmount",
        54 => "CannotSwapItem",
        55 => "CannotPlaceItem",
        56 => "UnhandledItemSetType",
        57 => "InvalidRemovedAmount",
        58 => "InvalidRegion",
        59 => "CannotDropItem",
        60 => "CannotDestroyItem",
        61 => "InvalidSourceContainer",
        62 => "ItemNotConsumed",
        63 => "InvalidNumCrafts",
        64 => "InvalidCraftResultStackSize",
        65 => "CannotRemoveItem",
        66 => "CannotConsumeItem",
        67 => "ScreenStackError",
        _ => "unknown status",
    }
}

/// `55 CannotPlaceItem`, or `200 unknown status (raw 200)` for a code the table does not list.
pub(crate) fn status_translated(code: u8) -> String {
    match status_name(code) {
        "unknown status" => format!("{code} unknown status (raw {code})"),
        name => format!("{code} {name}"),
    }
}

/// A refused `ItemStackRequest`: its id and the status the server answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub request_id: i32,
    pub code: u8,
}

impl Rejection {
    pub fn translated(&self) -> String {
        status_translated(self.code)
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request {} refused with status {}", self.request_id, self.translated())
    }
}

/// The refusal `response` carries, if the request was rejected.
pub(crate) fn rejection_of(response: &ItemStackResponsesItem) -> Option<Rejection> {
    match response.status {
        ItemStackResponsesItemStatus::Ok => None,
        status => Some(Rejection { request_id: response.request_id, code: status.to_raw() as u8 }),
    }
}

/// One `container/slot#stack` triple, as it goes out on the wire.
/// The whole structure we put on the wire, not a summary: relay only echoes `Status`/`RequestID`
/// and `ContainerInfo` is null, so this text is the only record of what the server actually saw.
/// The slot descriptors of every action in `packet`, one line per action.
///
/// This is what the server validated when it answered a request: a refusal such as
/// `50 FailedToValidateDstSlot` is about one of these triples, and without them a failure leaves the
/// next reader guessing which container or index the client actually named.
pub(crate) fn describe_request(packet: &ItemStackRequestPacket) -> Vec<String> {
    let mut lines = Vec::new();
    for request in &packet.requests {
        let id = request.request_id;
        let slot_text = |info: &StackRequestSlotInfo| describe_slot(info, id);
        for action in &request.actions {
            let line = match &action.content {
                Content::Place(place) => format!("Place {} <- {}", slot_text(&place.destination), slot_text(&place.source)),
                Content::Take(take) => format!("Take {} <- {}", slot_text(&take.destination), slot_text(&take.source)),
                Content::Swap(swap) => format!("Swap {} <-> {}", slot_text(&swap.source), slot_text(&swap.destination)),
                Content::Drop(drop) => format!("Drop {}", slot_text(&drop.source)),
                Content::Destroy(destroy) => format!("Destroy {}", slot_text(&destroy.source)),
                Content::Consume(consume) => format!("Consume {}", slot_text(&consume.source)),
                _ => format!("{:?}", action.type_id),
            };
            lines.push(line);
        }
    }
    lines
}

/// [`check_status`] with the slot descriptors of the refused request, so the message says what the
/// server was looking at (`… failed with status 50 — FailedToValidateDstSlot (sent: Swap …)`).
pub(crate) fn check_status_with_slots(response: &ItemStackResponsesItem, sent: &[String]) -> Result<(), ActionError> {
    match check_status(response) {
        Ok(()) => Ok(()),
        Err(ActionError::Rejected(message)) if !sent.is_empty() => {
            let mut text = format!("{message} (sent: {})", sent.join("; "));
            // t140: a `stack=#[req-N]` id is the request's own reference, not a stale stack id — say so,
            // otherwise the next reader reads it as a number the server should have known.
            if sent.iter().any(|line| names_request_stack(line)) {
                text.push_str(
                    " — stack=#[req(N)] names a stack this request creates: the server resolves it against its own response, so it is not a stale id",
                );
            }
            Err(ActionError::Rejected(text))
        }
        Err(other) => Err(other),
    }
}

pub(crate) fn check_status(response: &ItemStackResponsesItem) -> Result<(), ActionError> {
    match rejection_of(response) {
        None => Ok(()),
        Some(rejection) => Err(ActionError::Rejected(format!(
            "item stack request {} failed with status {} — {}",
            rejection.request_id,
            rejection.code,
            status_name(rejection.code)
        ))),
    }
}

impl Bot {
    /// The status of the last refused `ItemStackRequest`, cleared as it is read: callers that can
    /// answer a refusal with a different request shape (`equip`'s ladder) need the code, not the text.
    pub(crate) fn take_rejection(&mut self) -> Option<Rejection> {
        self.request_ids.last_rejection.take()
    }
}

#[cfg(test)]
mod t125_tests {
    use super::{status_name, status_translated};

    /// The four codes the acceptance names, plus one the table cannot know: an untranslated code
    /// must still say it is unknown instead of borrowing a neighbouring name.
    #[test]
    fn a_status_reads_as_its_protocol_name() {
        assert_eq!(status_name(0), "OK");
        assert_eq!(status_name(1), "Error");
        assert_eq!(status_name(2), "InvalidRequestActionType");
        // t123's refusal: the server could not carry out the move as it was asked.
        assert_eq!(status_name(55), "CannotPlaceItem");
        assert_eq!(status_translated(55), "55 CannotPlaceItem");
        assert_eq!(status_translated(0), "0 OK");
        assert_eq!(status_translated(2), "2 InvalidRequestActionType");
        assert_eq!(status_translated(200), "200 unknown status (raw 200)");
        assert_eq!(status_name(200), "unknown status");
    }
}
