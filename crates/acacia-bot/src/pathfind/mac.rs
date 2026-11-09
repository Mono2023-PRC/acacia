//! A macro move's controller: planned ones are stored on their path node, shortcuts are made by the follower.

use acacia_physics::WorldView;

use super::control::{Controller, Phase, RunCtl};
use super::leap::LeapCtl;
use super::profile::MovementProfile;
use super::rollout::Body;
use super::shortcut::ShortcutCtl;
use crate::movement::Controls;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Macro {
    Leap(LeapCtl),
    Run(RunCtl),
    Shortcut(ShortcutCtl),
}

impl Macro {
    pub fn profile(&self) -> MovementProfile {
        match self {
            Macro::Leap(l) => l.aim.profile,
            Macro::Run(r) => r.aim.profile,
            Macro::Shortcut(s) => s.aim.profile,
        }
    }

    /// Runs on the jitter stream `seed` instead of the one it was planned with.
    pub fn reseeded(mut self, seed: u64) -> Self {
        let aim = match &mut self {
            Macro::Leap(l) => &mut l.aim,
            Macro::Run(r) => &mut r.aim,
            Macro::Shortcut(s) => &mut s.aim,
        };
        *aim = aim.reseeded(seed);
        self
    }
}

impl Controller for Macro {
    fn step<W: WorldView + ?Sized>(&mut self, b: &Body, peek: &mut dyn FnMut(&Controls) -> Body, w: &W, c: &mut Controls) -> Phase {
        match self {
            Macro::Leap(l) => l.step(b, peek, w, c),
            Macro::Run(r) => r.step(b, peek, w, c),
            Macro::Shortcut(s) => s.step(b, peek, w, c),
        }
    }
}
