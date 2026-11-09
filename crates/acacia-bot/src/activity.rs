//! What the bot is doing right now, and how to stop all of it.
//!
//! Callers that drive a bot from the outside (a task layer, a chat command) need one answer to "is it
//! free?" and one call to "drop everything". Before this module those lived in three places: the
//! cancel/residue claims (which release what an action held), the survival state (item use), and the
//! movement controls. [`Bot::activity`] merges them into one [`Activity`]; [`Bot::stop_all`] is the
//! one call that ends whatever is running.
//!
//! The order in [`classify`] is the priority: what the bot is lying in, then what it is holding down,
//! then what it is standing in. Only one of them is reported, and a sleeping bot reports
//! [`Activity::Sleeping`] although its controls may still carry a stale key.

use crate::Bot;

/// What the bot is doing, for callers deciding whether to start something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    /// Nothing outstanding: no action, no key held, no screen, awake.
    Idle,
    /// Walking, sprinting or jumping (`goto`, a process, a caller's own controls).
    Walking,
    /// A block is being mined (`start_mining`, or a break inside `goto`/`build`).
    Mining,
    /// Holding "use" on a food, drink or other item (`consume`, auto-eat).
    UsingItem,
    /// In a bed, or between the bed click and the server's confirmation.
    Sleeping,
    /// Sitting in a vehicle (a boat the bot drives or one it is a passenger in).
    Riding,
    /// A container or the own inventory screen is open (item moves happen here).
    Container,
}

impl Activity {
    pub fn is_idle(self) -> bool {
        self == Activity::Idle
    }
}

/// The flags [`classify`] reads, in one place so the priority can be tested without a bot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Flags {
    sleeping: bool,
    using_item: bool,
    mining: bool,
    screen: bool,
    riding: bool,
    moving: bool,
}

/// One activity from the flags, by priority: sleeping, item use, mining, screen, riding, walking.
fn classify(f: Flags) -> Activity {
    if f.sleeping {
        Activity::Sleeping
    } else if f.using_item {
        Activity::UsingItem
    } else if f.mining {
        Activity::Mining
    } else if f.screen {
        Activity::Container
    } else if f.riding {
        Activity::Riding
    } else if f.moving {
        Activity::Walking
    } else {
        Activity::Idle
    }
}

impl Bot {
    /// What the bot is doing now (see [`Activity`]). Cheap: it reads state, it sends nothing.
    pub fn activity(&self) -> Activity {
        let moving = self.movement.as_ref().is_some_and(|m| {
            let c = &m.controls;
            c.forward != 0.0 || c.strafe != 0.0 || c.jump
        });
        classify(Flags {
            sleeping: self.is_sleeping(),
            using_item: self.is_using_item(),
            mining: self.mining.is_some(),
            screen: self.state.containers.open.is_some(),
            riding: self.vehicle().is_some(),
            moving,
        })
    }

    /// Whether anything is outstanding (the opposite of `activity() == Activity::Idle`).
    pub fn is_busy(&self) -> bool {
        !self.activity().is_idle()
    }

    /// Drops everything the bot is doing, so a caller can give it a new order:
    ///
    /// - cancels the action in flight (an awaited `goto`, `break_block`, `consume`, ... fails with
    ///   [`crate::ActionError::Cancelled`] at its next wait) via the bot's own [`crate::CancelHandle`];
    /// - releases what any action held: an in-flight break (`AbortBreak`), the own inventory screen
    ///   (closed), jump, pipelined placements, a bed (left);
    /// - lets go of every movement key and stops mining started with [`Bot::start_mining`].
    ///
    /// A **container** the caller opened stays open (it has a window id the caller may still need);
    /// close it with [`Bot::close_container`]. Call this from the same task that awaits the action: a
    /// cancelled action's own future must still be dropped or awaited before a new one starts.
    pub fn stop_all(&mut self) {
        self.cancel_handle().cancel();
        self.abandon_action();
        self.stop_mining();
        self.stop_pathing();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> Flags {
        Flags::default()
    }

    #[test]
    fn nothing_set_is_idle() {
        assert_eq!(classify(flags()), Activity::Idle);
        assert!(classify(flags()).is_idle());
        assert!(!Activity::Walking.is_idle());
    }

    #[test]
    fn sleeping_hides_every_other_flag() {
        let f = Flags { sleeping: true, using_item: true, mining: true, screen: true, riding: true, moving: true };
        assert_eq!(classify(f), Activity::Sleeping);
    }

    #[test]
    fn a_held_item_beats_mining_and_a_screen() {
        let f = Flags { using_item: true, mining: true, screen: true, ..flags() };
        assert_eq!(classify(f), Activity::UsingItem);
    }

    #[test]
    fn mining_beats_a_screen_and_walking() {
        let f = Flags { mining: true, screen: true, moving: true, ..flags() };
        assert_eq!(classify(f), Activity::Mining);
    }

    #[test]
    fn a_screen_beats_riding_and_walking() {
        let f = Flags { screen: true, riding: true, moving: true, ..flags() };
        assert_eq!(classify(f), Activity::Container);
    }

    #[test]
    fn riding_beats_walking() {
        let f = Flags { riding: true, moving: true, ..flags() };
        assert_eq!(classify(f), Activity::Riding);
    }
}
