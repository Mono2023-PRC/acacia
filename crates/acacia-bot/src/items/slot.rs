use acacia_client::proto::types::{ContainerSlotType, FullContainerName, StackRequestSlotInfo, WindowType};

use super::ui;
use crate::state::{Container, GameState, Inventory, ItemStack};

/// A slot the bot can move items from or to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotRef {
    /// The player's main inventory, 0-35: 0-8 are the hotbar, 9-35 the inventory.
    Main(u8),
    /// Helmet, chestplate, leggings, boots (0-3).
    Armor(u8),
    Offhand,
    /// The stack held by the mouse cursor.
    Cursor,
    /// A slot of the open container (chest, barrel, furnace, server GUI, ...).
    Container(u8),
    /// A workstation slot of the player's UI window by its offset (crafting grid, anvil,
    /// enchanting table, created output, ...; see [`crate::items::ui`]).
    Ui(u8),
}

impl SlotRef {
    /// Where craft results appear before they are moved out.
    pub const CREATED_OUTPUT: SlotRef = SlotRef::Ui(ui::CREATED_OUTPUT);

    pub fn hotbar(slot: u8) -> Self {
        SlotRef::Main(slot)
    }

    /// One of the player's own slots: main inventory, armor, offhand or cursor.
    pub fn is_player(self) -> bool {
        !matches!(self, SlotRef::Container(_) | SlotRef::Ui(_))
    }

    /// The tracked stack in this slot; `None` if the slot does not exist (or no container is open).
    pub fn stack(self, state: &GameState) -> Option<&ItemStack> {
        let inv = &state.inventory;
        match self {
            SlotRef::Main(i) => inv.main.get(usize::from(i)),
            SlotRef::Armor(i) => inv.armor.get(usize::from(i)),
            SlotRef::Offhand => Some(&inv.offhand),
            SlotRef::Cursor => Some(inv.cursor()),
            SlotRef::Container(i) => open_container(state)?.slots.get(usize::from(i)),
            SlotRef::Ui(i) => ui::slot_type(i).and_then(|_| inv.ui.get(usize::from(i))),
        }
    }

    pub(crate) fn stack_mut(self, state: &mut GameState) -> Option<&mut ItemStack> {
        let inv = &mut state.inventory;
        match self {
            SlotRef::Main(i) => inv.main.get_mut(usize::from(i)),
            SlotRef::Armor(i) => inv.armor.get_mut(usize::from(i)),
            SlotRef::Offhand => Some(&mut inv.offhand),
            SlotRef::Cursor => Some(&mut inv.ui[0]),
            SlotRef::Ui(i) => ui::slot_type(i).and_then(|_| inv.ui.get_mut(usize::from(i))),
            SlotRef::Container(i) => state.containers.open.as_mut().filter(|c| !is_own_screen(c))?.slots.get_mut(usize::from(i)),
        }
    }
}

/// The open non-player container, if any (opening the own inventory also shows up as open).
pub(crate) fn open_container(state: &GameState) -> Option<&Container> {
    state.containers.open.as_ref().filter(|c| !is_own_screen(c))
}

/// The own inventory screen: BDS opens it as the next window id (not 0) with type `Inventory`.
pub(crate) fn is_own_screen(container: &Container) -> bool {
    Inventory::is_player_window(container.window_id) || container.window_type == WindowType::Inventory
}

/// How a slot is named on the wire for the current screen. See [`SlotRef::wire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Screen {
    pub window_type: WindowType,
    pub block: BlockKind,
    /// How the offhand is named in a request's slot info; see [`OffhandAddress`].
    pub offhand: OffhandAddress,
}

/// How an `ItemStackRequest` names the offhand slot.
///
/// The offhand is a container of its own (`ContainerOffhand`, `mousetunnel/minecraft/protocol/
/// container.go`) holding exactly one stack, so a request addresses it as that container and index
/// 0. [`OffhandAddress::WindowSlot`] is the other numbering — the one a window/click uses — kept
/// because t127 was answered `50 FailedToValidateDstSlot` with it: if index 0 is refused too, the
/// alternate shape is the next thing to try before blaming the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OffhandAddress {
    /// `Offhand` + index 0: the one slot of the offhand container.
    #[default]
    ContainerSlot,
    /// `Offhand` + index 1: what the bot sent before t131 (Geyser's window numbering).
    WindowSlot,
    /// Neither: move the stack with two clicks (`Take` onto the cursor, `Place` into the offhand).
    /// t137 saw both numberings refused with `50 FailedToValidateDstSlot`, so the shape itself is the
    /// next thing to vary — `Plan` turns a swap into the two transfers when it sees this.
    ViaCursor,
}

impl OffhandAddress {
    /// How this numbering reads in a failure message.
    pub fn label(self) -> &'static str {
        match self {
            OffhandAddress::ContainerSlot => "offhand/0 (the offhand container's own slot)",
            OffhandAddress::WindowSlot => "offhand/1 (window numbering)",
            OffhandAddress::ViaCursor => "offhand by cursor (take + place)",
        }
    }

    pub fn slot(self) -> u8 {
        match self {
            OffhandAddress::ContainerSlot => 0,
            OffhandAddress::WindowSlot => 1,
            // Unused for this shape (the plan rewrites the swap), kept meaningful for diagnostics.
            OffhandAddress::ViaCursor => 0,
        }
    }

    /// The shape to try when the server refuses the current one with `FailedToValidateDstSlot`.
    pub fn alternate(self) -> Self {
        match self {
            OffhandAddress::ContainerSlot => OffhandAddress::WindowSlot,
            OffhandAddress::WindowSlot => OffhandAddress::ViaCursor,
            OffhandAddress::ViaCursor => OffhandAddress::ViaCursor,
        }
    }

    /// Two shapes: an exchange, then the two-click move. The second numbering is *not* a rung of its
    /// own — t137 showed the server refuses both numberings with `50 FailedToValidateDstSlot`, so what
    /// is left to vary is the action, not the index. A third guess is worth less than the evidence.
    pub fn ladder() -> [OffhandAddress; 2] {
        [OffhandAddress::ContainerSlot, OffhandAddress::ViaCursor]
    }

    /// Whether the plan has to express an offhand move as two clicks.
    pub fn via_cursor(self) -> bool {
        matches!(self, OffhandAddress::ViaCursor)
    }
}

/// Barrels and shulker boxes open as `WindowType::Container` like chests but use their own slot
/// type; only the block tells them apart, so this needs terrain tracking (physics bots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BlockKind {
    #[default]
    Other,
    Barrel,
    ShulkerBox,
}

impl BlockKind {
    pub fn from_name(name: &str) -> Self {
        if name == "minecraft:barrel" {
            BlockKind::Barrel
        } else if name.ends_with("shulker_box") {
            BlockKind::ShulkerBox
        } else {
            BlockKind::Other
        }
    }
}

impl Screen {
    /// Whether the offhand is addressed by moving the stack through the cursor (t138's second shape).
    pub fn offhand_via_cursor(&self) -> bool {
        self.offhand.via_cursor()
    }

    /// The same screen with the offhand named the other way; see [`OffhandAddress`].
    pub fn with_offhand(self, offhand: OffhandAddress) -> Self {
        Screen { offhand, ..self }
    }

    pub fn of(state: &GameState, block: BlockKind) -> Self {
        let open = open_container(state);
        Screen { window_type: open.map_or(WindowType::Inventory, |c| c.window_type), block, offhand: OffhandAddress::default() }
    }
}

impl SlotRef {
    /// Container name and slot index as sent in `ItemStackRequest`s, as vanilla names them; `auto`
    /// marks an automatic move (`request::auto_move`).
    pub(crate) fn wire(self, screen: &Screen, auto: bool) -> (ContainerSlotType, u8) {
        use ContainerSlotType as T;
        match self {
            // In the own inventory screen too: BDS rejects a recipe-book Consume from `Hotbar` (status 27).
            SlotRef::Main(i) if auto => (T::HotbarAndInventory, i),
            SlotRef::Main(i) if i < Inventory::HOTBAR_SLOTS => (T::Hotbar, i),
            SlotRef::Main(i) => (T::Inventory, i),
            SlotRef::Armor(i) => (T::Armor, i),
            // The offhand container holds one stack: index 0. (Index 1 is window numbering and was
            // answered `50 FailedToValidateDstSlot`, t127; `OffhandAddress` keeps both.)
            SlotRef::Offhand => (T::Offhand, screen.offhand.slot()),
            SlotRef::Cursor => (T::Cursor, 0),
            SlotRef::Container(i) => (container_slot_type(screen, i), i),
            SlotRef::Ui(i) => (ui::slot_type(i).unwrap_or(ContainerSlotType::Cursor), i),
        }
    }

    /// The request's view of this slot: container name, slot and the id of the stack it holds.
    pub(crate) fn slot_info(self, screen: &Screen, auto: bool, stack: &ItemStack) -> StackRequestSlotInfo {
        let stack_id = if stack.is_empty() { 0 } else { stack.stack_network_id.unwrap_or(0) };
        self.slot_info_with_id(screen, auto, stack_id)
    }

    pub(crate) fn slot_info_with_id(self, screen: &Screen, auto: bool, stack_id: i32) -> StackRequestSlotInfo {
        let (kind, slot) = self.wire(screen, auto);
        StackRequestSlotInfo { slot_type: FullContainerName { container_id: kind, dynamic_container_id: None }, slot, stack_id }
    }
}

/// A slot as one request names it, with the request's own stacks marked (t140).
///
/// Three kinds of `StackNetworkID` reach the wire and they mean different things:
///
/// - `0` — the slot is empty, so there is no stack id to name;
/// - a number the server assigned earlier (`stack=61`) — a stack the server already knows;
/// - **the id of the request being sent** (`stack=#[req(-5)]`) — a stack this very request creates, which
///   the server resolves against the response it is still building. Vanilla writes request ids into
///   empty/exchanged slots this way, so a refusal that shows `#[req(…)]` was not about a stale id.
///
/// Protocol basis: [`Plan`] marks such slots with `THIS_REQUEST` (`i32::MIN`, `items/plan.rs`) and
/// `Plan::request`'s substitution swaps in the real id before the packet goes out — so on the wire the
/// value is a plain positive request id and only this formatter can tell it apart from a stack id.
///
/// Writing the request id as a bare number made those two indistinguishable in the refusal text, which
/// is exactly the confusion t137's `50 FailedToValidateDstSlot` produced.
pub(crate) fn describe_slot(info: &StackRequestSlotInfo, request_id: i32) -> String {
    let container = &info.slot_type.container_id;
    let dynamic = match info.slot_type.dynamic_container_id {
        Some(id) => format!("dyn={id}"),
        None => "dyn=none".to_string(),
    };
    let stack = if info.stack_id == 0 {
        "stack=0 (empty)".to_string()
    } else if info.stack_id == request_id {
        format!("stack=#[req({request_id})]")
    } else {
        format!("stack={}", info.stack_id)
    };
    format!("{:?}({}) slot={} {stack} {dynamic}", container, container.to_raw(), info.slot)
}

/// Whether `line` names a stack this request creates (see [`describe_slot`]).
pub(crate) fn names_request_stack(line: &str) -> bool {
    line.contains("stack=#[req(")
}

fn container_slot_type(screen: &Screen, slot: u8) -> ContainerSlotType {
    use ContainerSlotType as T;
    use WindowType as W;
    let furnace = |ingredient| match slot {
        0 => ingredient,
        1 => T::FurnaceFuel,
        _ => T::FurnaceOutput,
    };
    match screen.window_type {
        W::Furnace => furnace(T::FurnaceIngredient),
        W::BlastFurnace => furnace(T::BlastFurnaceIngredient),
        W::Smoker => furnace(T::SmokerIngredient),
        W::BrewingStand => match slot {
            0 => T::BrewingInput,
            4 => T::BrewingFuel,
            _ => T::BrewingResult,
        },
        W::Crafter => T::Crafter,
        W::Container => match screen.block {
            BlockKind::Barrel => T::Barrel,
            BlockKind::ShulkerBox => T::Shulker,
            BlockKind::Other => T::Container,
        },
        _ => T::Container,
    }
}

#[cfg(test)]
mod t131_tests {
    use super::{describe_slot, names_request_stack, BlockKind, OffhandAddress, Screen, SlotRef};
    use acacia_client::proto::types::{FullContainerName, StackRequestSlotInfo};
    use acacia_client::proto::types::ContainerSlotType as T;

    fn screen() -> Screen {
        Screen { window_type: acacia_client::proto::types::WindowType::Inventory, block: BlockKind::Other, offhand: OffhandAddress::default() }
    }

    /// The offhand is its own container with a single stack, so a request names it with index 0 —
    /// the shape t127 never tried (it sent index 1, the window numbering, and got status 50).
    #[test]
    fn the_offhand_is_slot_zero_of_its_own_container() {
        assert_eq!(SlotRef::Offhand.wire(&screen(), false), (T::Offhand, 0));
        assert_eq!(SlotRef::Offhand.wire(&screen(), true), (T::Offhand, 0), "an automatic move names it the same way");
        // The other numbering stays reachable, so the caller can answer a refusal with it.
        let other = screen().with_offhand(OffhandAddress::WindowSlot);
        assert_eq!(SlotRef::Offhand.wire(&other, false), (T::Offhand, 1));
        assert_eq!(OffhandAddress::ContainerSlot.alternate(), OffhandAddress::WindowSlot);
        // t138: the ladder varies the *shape* after the numbering, so the second rung is the cursor.
        assert_eq!(OffhandAddress::WindowSlot.alternate(), OffhandAddress::ViaCursor);
        assert_eq!(OffhandAddress::ladder(), [OffhandAddress::ContainerSlot, OffhandAddress::ViaCursor]);
    }

    // ---- t140: three kinds of stack id, told apart in the message ----

    fn info(slot: u8, stack_id: i32) -> StackRequestSlotInfo {
        StackRequestSlotInfo {
            slot_type: FullContainerName { container_id: T::Offhand, dynamic_container_id: None },
            slot,
            stack_id,
        }
    }

    /// The id a request writes for a stack it creates is its own request id; unmarked it reads as a
    /// number the server should already know, which is how t137's refusal got misread.
    #[test]
    fn a_stack_this_request_creates_is_marked_with_the_request_id() {
        let line = describe_slot(&info(0, -5), -5);
        assert!(line.contains("stack=#[req(-5)]"), "{line}");
        assert!(names_request_stack(&line), "{line}");
        // A stack the server assigned earlier keeps its plain number and is *not* a request reference.
        let known = describe_slot(&info(0, 61), -5);
        assert!(known.contains("stack=61"), "{known}");
        assert!(!names_request_stack(&known), "{known}");
        // An empty slot says so instead of borrowing 0 as a stack id — and stays "(empty)" even when the
        // request id it would otherwise be compared against is 0, so nobody reads an empty slot as a
        // reference into the request.
        let empty = describe_slot(&info(0, 0), -5);
        assert!(empty.contains("stack=0 (empty)"), "{empty}");
        let empty_zero = describe_slot(&info(0, 0), 0);
        assert!(empty_zero.contains("stack=0 (empty)"), "{empty_zero}");
        assert!(!names_request_stack(&empty_zero), "an empty slot never references the request: {empty_zero}");
    }

    /// The same request id must not be mistaken for a reference when it names a different slot count:
    /// the mark depends on the id we actually sent, not on the id being non-zero.
    #[test]
    fn only_the_id_of_this_request_is_a_reference() {
        assert!(!names_request_stack(&describe_slot(&info(0, 7), 5)), "7 is a stack id, not this request");
        assert!(names_request_stack(&describe_slot(&info(0, 5), 5)), "5 is the request id");
    }

    /// The change must not move any other slot: hotbar and inventory keep their containers.
    #[test]
    fn the_two_numberings_read_differently_in_a_message() {
        assert!(OffhandAddress::ContainerSlot.label().contains("offhand/0"));
        assert!(OffhandAddress::WindowSlot.label().contains("offhand/1"));
        assert_ne!(OffhandAddress::ContainerSlot.label(), OffhandAddress::WindowSlot.label());
    }

    #[test]
    fn the_other_slots_keep_their_names() {
        assert_eq!(SlotRef::Main(3).wire(&screen(), false), (T::Hotbar, 3));
        assert_eq!(SlotRef::Main(20).wire(&screen(), false), (T::Inventory, 20));
        assert_eq!(SlotRef::Cursor.wire(&screen(), false), (T::Cursor, 0));
        assert_eq!(SlotRef::Armor(1).wire(&screen(), false), (T::Armor, 1));
    }
}
