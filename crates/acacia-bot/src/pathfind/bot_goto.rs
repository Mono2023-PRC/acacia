//! The pathfinding methods on [`Bot`].

use acacia_physics::BlockPos;

use super::goal::Goal;
use super::opts::PlanStats;
use super::goto::GotoOpts;
use super::process::{Goto, Processes};
use crate::{ActionError, Bot};

impl Bot {
    /// Walks to `goal` (physics bots only). See [`Navigator`](super::Navigator) for step-wise control.
    pub async fn goto(&mut self, goal: Goal) -> Result<(), ActionError> {
        self.goto_with(goal, GotoOpts::default()).await
    }

    /// [`Bot::run`] with a single [`Goto`].
    pub async fn goto_with(&mut self, goal: Goal, opts: GotoOpts) -> Result<(), ActionError> {
        // A walk that never gets refused must not leave the previous walk's numbers lying around as if they were
        // this one's evidence (t117; the same trap `jump_count` had).
        self.clear_plan_stats();
        let mut procs = Processes::new();
        let id = procs.add(Goto::with(goal, opts), 0);
        self.run(&mut procs).await?;
        procs.take_outcome(id).unwrap_or(Ok(()))
    }

    /// Releases the movement controls, e.g. after dropping a `goto` future or a [`Navigator`].
    pub fn stop_pathing(&mut self) {
        if let Some(c) = self.controls() {
            c.stop();
        }
    }

    /// What the last search did (t117, widened in t150): `None` only when no search has run since the walk started.
    ///
    /// The pilot writes this both when it gives up and when it arrives, so a caller can read *which engine planned the
    /// walk* (`engine_used`, `fallback_reason`), what it cost (`cost`) and whether it was incremental (`reused`) —
    /// numbers instead of prose, on the successful path too. `budget_exhausted` and `closest_short` still separate
    /// "the search was cut short" from "the goal is not connected to here" when the walk was refused.
    pub fn last_plan_stats(&self) -> Option<PlanStats> {
        self.plan_stats.lock().ok().and_then(|slot| *slot)
    }

    /// Takes the last search's report, leaving the slot empty (t150, the [`crate::survival::EquipReport`] precedent):
    /// a caller that reads it *and* owns the next action should drain it, so a later reader cannot mistake this walk's
    /// numbers for its own.
    pub fn take_plan_stats(&self) -> Option<PlanStats> {
        drain_stats(&self.plan_stats)
    }

    /// Forgets the last search: called at the start of every [`Bot::goto_with`].
    pub(crate) fn clear_plan_stats(&self) {
        if let Ok(mut slot) = self.plan_stats.lock() {
            *slot = None;
        }
    }

    /// Feet block position of a physics bot (for building goals).
    pub fn block_position(&self) -> Option<BlockPos> {
        let p = self.movement.as_ref()?.position()?;
        Some([p[0].floor() as i32, (p[1] + 0.01).floor() as i32, p[2].floor() as i32])
    }
}

/// `take` on the shared slot, split out so its one-shot semantics can be tested without a server (t150).
fn drain_stats(slot: &std::sync::Arc<std::sync::Mutex<Option<PlanStats>>>) -> Option<PlanStats> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

#[cfg(test)]
mod t150_tests {
    use std::sync::{Arc, Mutex};

    use super::drain_stats;
    use crate::pathfind::PlanStats;

    fn stats(cost: f32, reused: bool) -> PlanStats {
        PlanStats { nodes_expanded: 7, max_nodes: 200_000, cost, reused, engine_used: PlanStats::DSTAR, ..PlanStats::default() }
    }

    #[test]
    fn the_report_hands_the_last_search_over_exactly_once() {
        let slot = Arc::new(Mutex::new(Some(stats(12.5, true))));
        assert_eq!(drain_stats(&slot), Some(stats(12.5, true)), "the numbers survive the hand-over");
        assert_eq!(drain_stats(&slot), None, "a drained slot cannot be read twice");
        assert_eq!(drain_stats(&slot), None, "and stays empty");
    }

    #[test]
    fn an_empty_slot_reads_as_nothing_searched_yet() {
        let slot = Arc::new(Mutex::new(None));
        assert_eq!(drain_stats(&slot), None);
    }
}
