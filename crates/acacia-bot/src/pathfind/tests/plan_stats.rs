//! What a refused search says about itself (t117).
//!
//! t116 asked for a goal 40 blocks down a corridor and got `every reachable cell searched` after 0.077 s with the body
//! still standing at the start. Nothing in that message said whether the goal was walled off (change the goal) or the
//! search stopped early (raise the budget, walk in stages), and those are different instructions. These pin the
//! numbers that tell them apart, and the wording that follows from them.

use super::grid::Grid;
use super::search::opts;
use crate::pathfind::pilot::unreachable_message;
use crate::pathfind::{Goal, MovementProfile, SearchOpts, search};

/// A flat stone lane from x 0 to 9, enough room for the body.
fn lane() -> Grid {
    let g = Grid::new();
    g.fill([0, 0, -2], [9, 0, 2], "stone");
    g
}

/// The same lane closed at x = 3 by a wall the body cannot climb.
fn walled_lane() -> Grid {
    let g = lane();
    g.fill([3, 1, -2], [3, 3, 2], "stone");
    g
}

/// The lane with the goal cell itself solid.
fn blocked_goal() -> Grid {
    let g = lane();
    g.fill([9, 1, 0], [9, 2, 0], "stone");
    g
}

const START: [f32; 3] = [0.5, 1.0, 0.5];

fn walked_short(plan: crate::pathfind::PlanStats) -> String {
    unreachable_message(&Goal::Block([9, 1, 0]), START.into(), plan)
}

#[test]
fn a_goal_on_the_far_side_of_a_wall_is_reported_as_searched_out_but_far_short() {
    // A wall the body cannot climb closes the lane at x = 3: the search does see everything it can reach, and it never
    // comes near the goal. That is "the two areas are not connected", not "the search gave up" and not "the goal is
    // walled in" -- the closest-approach number is what says so.
    let g = walled_lane();
    let p = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &opts(true));
    assert!(p.unreachable, "the lane is closed and fully loaded");
    assert!(!p.plan.budget_exhausted, "nothing cut it short: {} nodes of {}", p.plan.nodes_expanded, p.plan.max_nodes);
    assert!(!p.plan.target_region_reached, "the goal is six blocks past the wall");
    assert_eq!(p.plan.closest_short, Some(6.0), "the last standable cell before the wall is x = 2, seven blocks off, six past the region slack");
    let message = walked_short(p.plan);
    assert!(message.contains("every reachable cell searched"), "{message}");
    assert!(message.contains("6.0 blocks short"), "the number has to be in the text: {message}");
}

#[test]
fn a_goal_whose_neighbourhood_was_reached_without_a_cell_to_stand_on_is_the_other_kind() {
    // The goal cell itself is stone: the search stands next to it (1 block away, inside the goal's neighbourhood) and
    // still cannot finish. Now "every reachable cell searched, none of them on the goal" is the true sentence.
    let g = blocked_goal();
    let p = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &opts(true));
    assert!(p.unreachable, "no cell of the goal is standable");
    assert!(!p.plan.budget_exhausted);
    assert!(p.plan.target_region_reached, "x = 8 is inside the goal's neighbourhood: {:?}", p.plan);
    assert_eq!(p.plan.closest_short, Some(0.0));
    let message = walked_short(p.plan);
    assert!(message.contains("none of them on the goal"), "{message}");
}

#[test]
fn a_search_cut_by_its_node_budget_never_claims_to_have_searched_everything() {
    // One node of budget on an open lane: the search stops immediately and has learned nothing about the goal.
    let g = lane();
    let tight = SearchOpts { max_nodes: 1, ..opts(true) };
    let p = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &tight);
    assert!(!p.complete, "one node cannot reach nine blocks away");
    assert!(p.plan.budget_exhausted, "max_nodes = 1 has to show up as the reason: {:?}", p.plan);
    assert_eq!(p.plan.max_nodes, 1, "the budget in the report is the one in effect, not the default");
    assert_eq!(p.plan.nodes_expanded, 1);
    let message = walked_short(p.plan);
    assert!(!message.contains("every reachable cell searched"), "a cut search may not claim coverage: {message}");
    assert!(message.contains("ran out of budget") && message.contains("max 1"), "{message}");

    // The same lane with room to think: not cut, not unreachable, no refusal to word.
    let roomy = SearchOpts { max_nodes: 50_000, ..opts(true) };
    let p = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &roomy);
    assert!(p.complete && !p.plan.budget_exhausted, "{:?}", p.plan);
    assert_eq!(p.plan.max_nodes, 50_000);
}

#[test]
fn the_report_carries_the_names_a_tool_layer_reads() {
    let g = walled_lane();
    let p = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &opts(true));
    let json = p.plan.to_json();
    for key in ["nodes_expanded", "max_nodes", "budget_exhausted", "target_region_reached", "closest_short"] {
        assert!(!json[key].is_null(), "{key} missing from {json}");
    }
    assert_eq!(json["budget_exhausted"], serde_json::json!(false));
    assert_eq!(json["target_region_reached"], serde_json::json!(false));
    // and the refusal text ends with exactly that object, so a consumer can parse it either way
    let message = walked_short(p.plan);
    let tail = message.rsplit_once("[plan ").expect("the plan object rides along").1;
    assert!(tail.starts_with('{') && tail.ends_with(']'), "{message}");
    assert!(tail.contains("\"nodes_expanded\""), "{message}");
    // The profile knob does not change the report's meaning.
    let human = search(&g.terrain(), START, &Goal::Block([9, 1, 0]), &SearchOpts { profile: MovementProfile::Human, ..opts(true) });
    assert!(human.plan.nodes_expanded > 0);
}

/// A search answer built by hand: the pilot's bookkeeping is what is under test, not the search.
fn answer(unreachable: bool, plan: crate::pathfind::PlanStats) -> crate::pathfind::Path {
    crate::pathfind::Path {
        nodes: Vec::new(),
        complete: true,
        unreachable,
        cost: 0.0,
        visited: plan.nodes_expanded,
        macro_stats: Default::default(),
        plan,
    }
}

/// The slot a tool layer reads (`Bot::last_plan_stats`) has to describe **the last search**, refused or not, and never
/// keep the previous one — a stale report is worse than no report, which is the trap `jump_count` already fell into
/// (t117). t150 widened it: a successful search reports itself too, so "which engine planned this, what did it cost,
/// was it incremental" is readable on the happy path instead of only after a failure.
#[test]
fn a_refusal_fills_the_slot_and_the_next_accepted_path_replaces_it() {
    use std::sync::{Arc, Mutex};

    use crate::pathfind::{GotoOpts, Pilot};

    let slot = Arc::new(Mutex::new(None));
    let mut pilot = Pilot::new(Goal::Block([9, 1, 0]), GotoOpts::default()).reporting(slot.clone());
    assert_eq!(slot.lock().unwrap().to_owned(), None, "a fresh pilot has nothing to report");

    let plan = crate::pathfind::PlanStats { nodes_expanded: 42, max_nodes: 200_000, budget_exhausted: false, target_region_reached: false, closest_short: Some(6.0), ..crate::pathfind::PlanStats::default() };
    let refused = pilot.accept(answer(true, plan), START.into());
    assert!(refused.is_err(), "an unreachable answer is a refusal");
    assert_eq!(slot.lock().unwrap().to_owned(), Some(plan), "the numbers the message carries are readable as numbers");

    let walked = crate::pathfind::PlanStats { nodes_expanded: 1, max_nodes: 200_000, budget_exhausted: false, target_region_reached: true, closest_short: Some(0.0), cost: 9.5, reused: true, ..crate::pathfind::PlanStats::default() };
    let accepted = pilot.accept(answer(false, walked), START.into());
    assert!(accepted.is_ok(), "a reachable answer is taken");
    let now = slot.lock().unwrap().to_owned().expect("a walk that arrived still says how it was planned");
    assert_eq!(now, walked, "the slot describes this walk, price and all");
    assert_eq!((now.cost, now.reused), (9.5, true), "the numbers a caller reads are this search's");
    // ...and nothing of the refusal survives: not its distance, not its verdict about the neighbourhood. Replacing the
    // statistics in place would leave a caller unable to tell "9.5, reached" from "9.5, still 6 blocks short".
    assert_ne!(now, plan, "the refusal's report is gone, not edited in place");
    assert_ne!(now.closest_short, plan.closest_short, "the distance the refusal measured does not leak into the walk");
    assert!(now.target_region_reached && !plan.target_region_reached, "the two reports disagree on purpose");
}
