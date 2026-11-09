use super::*;

#[test]
fn hostile_kinds_are_recognised_with_and_without_the_namespace() {
    for kind in ["minecraft:zombie", "zombie", "minecraft:wither_skeleton", "minecraft:warden", "minecraft:ender_dragon"] {
        assert!(is_hostile(kind), "{kind} is hostile");
    }
    for kind in ["minecraft:cow", "minecraft:pig", "minecraft:player", "minecraft:piglin", "minecraft:enderman", "minecraft:villager", ""] {
        assert!(!is_hostile(kind), "{kind} is not");
    }
}

#[test]
fn the_nearest_enemy_wins_and_the_radius_filters() {
    let drops = [(10, [3.0, 0.0, 0.0]), (11, [1.5, 0.0, 0.0]), (12, [9.0, 0.0, 0.0])];
    assert_eq!(nearest_by(drops.into_iter(), [0.0, 0.0, 0.0], 5.0), Some((11, 1.5)));
    assert_eq!(nearest_by(drops.into_iter(), [0.0, 0.0, 0.0], 2.0), Some((11, 1.5)));
    assert_eq!(nearest_by(drops.into_iter(), [0.0, 0.0, 0.0], 1.0), None);
    // Distance is 3-D: something 4 blocks up is out of a 3-block radius.
    assert_eq!(nearest_by(std::iter::once((7, [0.0, 4.0, 0.0])), [0.0, 0.0, 0.0], 3.0), None);
}

#[test]
fn ties_go_to_the_first_entry() {
    let tied = [(1, [1.0, 0.0, 0.0]), (2, [-1.0, 0.0, 0.0])];
    assert_eq!(nearest_by(tied.into_iter(), [0.0, 0.0, 0.0], 5.0), Some((1, 1.0)));
}

#[test]
fn the_default_strikes_inside_the_servers_reach() {
    let opts = CombatOpts::default();
    assert!(opts.range < ENTITY_REACH, "{} must stay under {ENTITY_REACH}", opts.range);
    assert!(opts.range > 1.5, "attacking from further than arm's length looks wrong");
    assert_eq!(opts.retreat_below, Some(5.0));
}
