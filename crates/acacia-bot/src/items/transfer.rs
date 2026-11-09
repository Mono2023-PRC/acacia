//! Bulk container moves on top of [`Bot::open_container_at`] and the item-stack primitives:
//! deposit whole stacks from the inventory into a container, and pull a number of items out.
//!
//! Both are compositions of what already existed — [`Bot::quick_move`] picks the destination slots
//! the way a shift-click does (top up a matching stack, then the first empty slot) and the server
//! answers with an `ItemStackResponse` — so the interesting parts here are: finding the container
//! block, opening it once, choosing *which* inventory stacks to give up, and stopping cleanly when
//! the container (or the inventory) is full instead of failing the whole call.
//!
//! The window bookkeeping is the relay's: [`Bot::open_container_at`] waits for `ContainerOpen`, and
//! the item requests name the open window id. If a server (Geyser, a chest-shop GUI) never answers a
//! request, see `docs/relay-needs.md`.

use acacia_physics::BlockPos;
use acacia_world::BlockAccess;

use crate::items::SlotRef;
use crate::state::{Container, Inventory};
use crate::{ActionError, Bot};

/// Blocks [`Bot::find_container`] looks for and [`Bot::deposit`]/[`Bot::withdraw`] open: the plain
/// storage blocks (the furnace family is a workstation, see [`crate::workstation`]).
const CONTAINER_BLOCKS: &[&str] = &[
    "chest",
    "trapped_chest",
    "barrel",
    "ender_chest",
    "shulker_box",
    "white_shulker_box",
    "orange_shulker_box",
    "magenta_shulker_box",
    "light_blue_shulker_box",
    "yellow_shulker_box",
    "lime_shulker_box",
    "pink_shulker_box",
    "gray_shulker_box",
    "light_gray_shulker_box",
    "cyan_shulker_box",
    "purple_shulker_box",
    "blue_shulker_box",
    "brown_shulker_box",
    "green_shulker_box",
    "red_shulker_box",
    "black_shulker_box",
];

/// Item moves [`Bot::withdraw`] makes before returning what it got: a stack per request.
const MAX_WITHDRAW_MOVES: usize = 36;

/// Whether `block` (a registry name such as `minecraft:chest`) is a container these calls can open.
pub fn is_container_block(block: &str) -> bool {
    let bare = block.strip_prefix("minecraft:").unwrap_or(block);
    CONTAINER_BLOCKS.contains(&bare)
}

impl Bot {
    /// The nearest container block within `radius` blocks of the feet (`y` within 3), with its block
    /// position and registry name. Physics bots with terrain tracking only.
    pub fn find_container(&self, radius: i32) -> Option<(BlockPos, &'static str)> {
        let feet = self.block_position()?;
        let world = self.world.as_ref()?;
        let (view, registry) = (world.view()?, world.registry()?);
        let mut best: Option<(i32, BlockPos, &'static str)> = None;
        for x in feet[0] - radius..=feet[0] + radius {
            for y in (feet[1] - 3).max(-64)..=(feet[1] + 3).min(320) {
                for z in feet[2] - radius..=feet[2] + radius {
                    let Some(state) = registry.get(view.block(x, y, z)) else { continue };
                    if !is_container_block(state.name) {
                        continue;
                    }
                    let d = (x - feet[0]).pow(2) + (y - feet[1]).pow(2) + (z - feet[2]).pow(2);
                    if best.is_none_or(|(bd, _, _)| d < bd) {
                        best = Some((d, [x, y, z], state.name));
                    }
                }
            }
        }
        best.map(|(_, pos, name)| (pos, name))
    }

    /// Opens the container at `pos` unless it is already the open window.
    ///
    /// `NotPossible` when the block there is not a container these calls know (or is not loaded), and
    /// whatever [`Bot::open_container_at`] returns when the server refuses or never opens it.
    pub async fn open_or_focus_container(&mut self, pos: BlockPos) -> Result<(), ActionError> {
        if self.open_container().is_some_and(|c| c.position.as_ref().is_some_and(|p| [p.x, p.y, p.z] == pos)) {
            return Ok(());
        }
        if let Some((_, block)) = self.block_at(pos) {
            if !is_container_block(block.name) {
                return Err(ActionError::NotPossible(format!("{} at {pos:?} is not a container these calls open", block.name)));
            }
        }
        if self.state.containers.open.is_some() {
            self.close_container().await?;
        }
        self.open_container_at(pos).await.map(|_| ())
    }

    /// Moves whole stacks from the inventory into the container at `pos`: every stack whose item is in
    /// `only` (an empty `only` takes everything). Hotbar first, like a shift-click. Stops when the
    /// container has no room and returns how many stacks moved; the container is left open.
    pub async fn deposit(&mut self, pos: BlockPos, only: &[&str]) -> Result<usize, ActionError> {
        let mut ids: Vec<i32> = Vec::with_capacity(only.len());
        for name in only {
            let id = self
                .state
                .items
                .id(name)
                .ok_or_else(|| ActionError::NotPossible(format!("unknown item {name:?} (is the item registry loaded?)")))?;
            ids.push(id);
        }
        self.open_or_focus_container(pos).await?;
        let slots = deposit_slots(&self.state.inventory, &ids);
        if slots.is_empty() {
            return Err(ActionError::NotPossible(if only.is_empty() {
                "the inventory is empty".into()
            } else {
                format!("no stack of {only:?} in the inventory")
            }));
        }
        let mut moved = 0;
        for slot in slots {
            match self.quick_move(SlotRef::Main(slot)).await {
                Ok(_) => moved += 1,
                // The container filled up (or the stack no longer fits whole): keep what went in.
                Err(ActionError::NotPossible(_)) => break,
                Err(e) => return Err(e),
            }
        }
        tracing::debug!(?pos, moved, "deposit");
        Ok(moved)
    }

    /// [`Bot::deposit`] of every stack in the inventory.
    pub async fn deposit_all(&mut self, pos: BlockPos) -> Result<usize, ActionError> {
        self.deposit(pos, &[]).await
    }

    /// Takes up to `count` of `item` out of the container at `pos` into the inventory, a stack per
    /// quick-move, stopping when the container holds no more or the inventory is full. Returns how many
    /// were moved; the container is left open.
    ///
    /// `NotPossible` when `item` is unknown or the container holds none of it, `Rejected` when the
    /// server took a request but the count did not rise.
    pub async fn withdraw(&mut self, pos: BlockPos, item: &str, count: u32) -> Result<u32, ActionError> {
        let id = self
            .state
            .items
            .id(item)
            .ok_or_else(|| ActionError::NotPossible(format!("unknown item {item:?} (is the item registry loaded?)")))?;
        self.open_or_focus_container(pos).await?;
        let before = self.state.inventory.count_of(id);
        if withdraw_slot(self.open_container(), id).is_none() {
            return Err(ActionError::NotPossible(format!("the container at {pos:?} holds no {item}")));
        }
        for _ in 0..MAX_WITHDRAW_MOVES {
            let Some(slot) = withdraw_slot(self.open_container(), id) else { break };
            match self.quick_move(SlotRef::Container(slot)).await {
                Ok(_) => {}
                // No room in the inventory (or the stack stopped existing): stop with what we have.
                Err(ActionError::NotPossible(_)) => break,
                Err(e) => return Err(e),
            }
            let now = self.state.inventory.count_of(id);
            if now <= before {
                // The server accepted a move that did not raise the count: stop rather than spin.
                break;
            }
            if now - before >= count {
                break;
            }
        }
        let got = self.state.inventory.count_of(id).saturating_sub(before);
        tracing::debug!(?pos, item, got, "withdraw");
        if got == 0 { Err(ActionError::Rejected(format!("the container did not give up any {item}"))) } else { Ok(got) }
    }
}

/// Main-inventory slots to deposit, hotbar first: every non-empty slot when `ids` is empty, else the
/// slots holding one of `ids`. A slot whose stack does not match is skipped, not an error.
fn deposit_slots(inv: &Inventory, ids: &[i32]) -> Vec<u8> {
    let mut slots: Vec<u8> = (0..inv.main.len())
        .filter(|&i| {
            let stack = &inv.main[i];
            !stack.is_empty() && (ids.is_empty() || ids.contains(&stack.network_id))
        })
        .map(|i| u8::try_from(i).unwrap_or(u8::MAX))
        .collect();
    // A shift-click gives up the hotbar first (the selected slot before the rest), then the inventory.
    slots.sort_by_key(|&slot| {
        let hotbar = u32::from(slot) < u32::from(Inventory::HOTBAR_SLOTS);
        (u32::from(!hotbar), u32::from(slot))
    });
    slots
}

/// The first container slot holding `id`.
fn withdraw_slot(open: Option<&Container>, id: i32) -> Option<u8> {
    open?.slots.iter().position(|s| !s.is_empty() && s.network_id == id).and_then(|i| u8::try_from(i).ok())
}

#[cfg(test)]
mod tests {
    use acacia_client::proto::types::{BlockCoordinates, WindowType};

    use super::*;
    use crate::state::{Container, GameState, ItemStack};

    fn stack(network_id: i32, count: u16) -> ItemStack {
        ItemStack { network_id, count, ..ItemStack::default() }
    }

    fn state_with(items: &[(usize, ItemStack)]) -> GameState {
        let mut state = GameState::default();
        for (slot, item) in items {
            state.inventory.main[*slot] = item.clone();
        }
        state
    }

    fn container(slots: Vec<ItemStack>) -> Container {
        Container {
            window_id: 1,
            window_type: WindowType::Container,
            position: Some(BlockCoordinates { x: 1, y: 64, z: 2 }),
            entity: None,
            title: None,
            slots,
        }
    }

    #[test]
    fn storage_blocks_are_recognised_and_workstations_are_not() {
        assert!(is_container_block("minecraft:chest"));
        assert!(is_container_block("chest"));
        assert!(is_container_block("minecraft:light_blue_shulker_box"));
        assert!(!is_container_block("minecraft:furnace"));
        assert!(!is_container_block("minecraft:stone"));
    }

    #[test]
    fn everything_goes_in_hotbar_first() {
        let state = state_with(&[(0, stack(5, 3)), (12, stack(6, 1)), (30, stack(5, 2))]);
        assert_eq!(deposit_slots(&state.inventory, &[]), vec![0, 12, 30]);
    }

    #[test]
    fn only_the_named_items_go_and_empty_slots_are_skipped() {
        let state = state_with(&[(0, stack(5, 3)), (12, stack(6, 1)), (30, stack(5, 2))]);
        assert_eq!(deposit_slots(&state.inventory, &[6]), vec![12]);
        assert_eq!(deposit_slots(&state.inventory, &[5]), vec![0, 30]);
        assert!(deposit_slots(&state.inventory, &[9]).is_empty());
    }

    #[test]
    fn an_empty_inventory_deposits_nothing() {
        assert!(deposit_slots(&GameState::default().inventory, &[]).is_empty());
    }

    #[test]
    fn withdrawal_takes_the_first_slot_holding_the_item() {
        let open = container(vec![stack(5, 10), stack(0, 0), stack(5, 2)]);
        assert_eq!(withdraw_slot(Some(&open), 5), Some(0));
        assert_eq!(withdraw_slot(Some(&open), 7), None);
        assert_eq!(withdraw_slot(None, 5), None);
    }
}
