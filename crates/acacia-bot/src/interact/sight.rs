//! What the eye can see: a ray is stopped by any block shape a vanilla crosshair or an anticheat ray would
//! stop on, and the bot aims only at points it sees (research/anticheat-interaction-checks.md).

use acacia_physics::{BlockPos, Vec3};
use acacia_world::ray::{self, RayHit};
use acacia_world::{selection, Aabb, BlockAccess, BlockRegistry};

use super::geometry::Face;

/// Clicks within this of the ray's end still count as reaching it.
const SLACK: f64 = 1e-3;

pub(crate) struct Sight<'a, W: BlockAccess> {
    pub world: &'a W,
    pub registry: &'a BlockRegistry,
}

impl<W: BlockAccess> Sight<'_, W> {
    /// The first block shape on the way from `eye` towards `to`, at most `extra` past it; the cell `through` is
    /// passed through.
    fn cast(&self, eye: Vec3, to: Vec3, extra: f64, through: Option<BlockPos>) -> Option<RayHit> {
        let (origin, d) = (eye.map(f64::from), [to[0] - eye[0], to[1] - eye[1], to[2] - eye[2]].map(f64::from));
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len < 1e-9 {
            return None;
        }
        // Collision stops an anticheat's ray, selection the vanilla crosshair's: the bot respects both.
        let shapes = |pos: [i32; 3], id: u32, out: &mut Vec<Aabb>| {
            if let Some(s) = self.registry.get(id).filter(|_| through != Some(pos)) {
                out.extend(s.boxes.iter().copied().chain(selection::boxes_at(self.world, self.registry, pos)));
            }
        };
        ray::cast(self.world, origin, d.map(|c| c / len), len + extra, shapes)
    }

    /// No block shape lies between `eye` and `point` (an entity's hit point).
    pub fn clear(&self, eye: Vec3, point: Vec3) -> bool {
        self.cast(eye, point, -SLACK, None).is_none()
    }

    /// The ray from `eye` through `point` first meets the block at `pos` on `face`.
    pub fn sees_face_at(&self, eye: Vec3, pos: BlockPos, face: Face, point: Vec3) -> bool {
        self.sees_face_into(eye, pos, face, point, None)
    }

    /// The eye sees the centre of `face` of the block at `pos`: what a fill's placement against that face checks.
    pub fn sees_face_centre(&self, eye: Vec3, pos: BlockPos, face: Face) -> bool {
        self.sees_face_at(eye, pos, face, super::geometry::face_point(pos, face))
    }

    /// [`Self::sees_face_at`] for a placement that merges into `into` (a half slab made double): what is there now
    /// does not hide the face.
    pub fn sees_face_into(&self, eye: Vec3, pos: BlockPos, face: Face, point: Vec3, into: Option<BlockPos>) -> bool {
        self.cast(eye, point, 0.05, into).is_some_and(|h| h.block == pos && Face::from_wire(h.face) == face)
    }

    /// Faces of the block at `pos` the eye sees, each with the world points on it the eye reaches unblocked,
    /// nearest first. Any exposed face a ray reaches counts: every probe on every face of every box of the
    /// shape is cast through the world's voxel walk, so a block seen past an edge (a hole's rim, a wall's end,
    /// the sliver of a face above the floor the bot stands on) is not missed.
    pub fn visible_points(&self, eye: Vec3, pos: BlockPos) -> Vec<(Face, Vec3)> {
        let Some(state) = self.registry.get(self.world.block(pos[0], pos[1], pos[2])) else { return Vec::new() };
        let selection = selection::boxes_at(self.world, self.registry, pos);
        let boxes = if selection.is_empty() { state.boxes.to_vec() } else { selection };
        let mut seen: Vec<(Face, Vec3)> = Vec::new();
        for b in &boxes {
            for face in Face::ALL {
                for p in face_probes(b, face) {
                    let to = [pos[0] as f32 + p[0], pos[1] as f32 + p[1], pos[2] as f32 + p[2]];
                    let Some(hit) = self.cast(eye, to, 0.05, None).filter(|h| h.block == pos) else { continue };
                    if Face::from_wire(hit.face) != face {
                        continue;
                    }
                    let at = hit.point.map(|c| c as f32);
                    if !seen.iter().any(|&(_, q): &(Face, Vec3)| q == at) {
                        seen.push((face, at));
                    }
                }
            }
        }
        let distance = |p: Vec3| {
            let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
            d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
        };
        seen.sort_by(|a, b| distance(a.1).total_cmp(&distance(b.1)));
        seen
    }

    /// The block standing between `eye` and the block at `pos`: of the rays aimed at one of `pos`'s faces, the
    /// nearest thing they meet instead (t101). `None` when no ray is obstructed — then nothing is to blame and the
    /// caller must not name one.
    ///
    /// The live run that asked for this: nine `not_visible` failures, five of them the *same* cell retried
    /// (`dig [150026, 2, 149828]`, a diamond ore the bot had just found), because the message said only "behind
    /// other blocks". Naming the occluder turns a guess into a next step: dig that one instead.
    pub fn occluder(&self, eye: Vec3, pos: BlockPos) -> Option<BlockPos> {
        let Some(state) = self.registry.get(self.world.block(pos[0], pos[1], pos[2])) else { return None };
        let selection = selection::boxes_at(self.world, self.registry, pos);
        let boxes = if selection.is_empty() { state.boxes.to_vec() } else { selection };
        let mut best: Option<(f64, BlockPos)> = None;
        for b in &boxes {
            for face in Face::ALL {
                for p in face_probes(b, face) {
                    let to = [pos[0] as f32 + p[0], pos[1] as f32 + p[1], pos[2] as f32 + p[2]];
                    let Some(hit) = self.cast(eye, to, 0.05, None) else { continue };
                    if hit.block == pos {
                        continue;
                    }
                    // Distance along the ray, so the nearest obstacle is named even when several are in line.
                    let d = {
                        let p = hit.point;
                        let d = [p[0] - eye[0] as f64, p[1] - eye[1] as f64, p[2] - eye[2] as f64];
                        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
                    };
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, hit.block));
                    }
                }
            }
        }
        best.map(|(_, block)| block)
    }

}

/// Probes per face axis: a face seen through a narrow gap still counts (the client aims at what its crosshair
/// hits, not at the box's corners).
const PROBES: usize = 4;
/// How far inside the box a probe sits, so the ray enters through the face it was aimed at.
const PROBE_INSET: f32 = 1e-3;

/// Block-local points on `face` of `b`, a [`PROBES`]×[`PROBES`] grid nudged just inside the box.
fn face_probes(b: &Aabb, face: Face) -> impl Iterator<Item = Vec3> + '_ {
    let normal = face.offset();
    let axis = (0..3).find(|&i| normal[i] != 0).expect("a face has an axis");
    let (u, w) = ((axis + 1) % 3, (axis + 2) % 3);
    let plane = if normal[axis] > 0 { b.max[axis] } else { b.min[axis] };
    // Just inside the box, so a ray aimed at this face enters through it.
    let plane = plane - if normal[axis] > 0 { PROBE_INSET } else { -PROBE_INSET };
    (0..PROBES).flat_map(move |i| {
        (0..PROBES).map(move |j| {
            let at = |k: usize, n: usize| b.min[k] + (b.max[k] - b.min[k]) * (n as f32 + 0.5) / PROBES as f32;
            let mut p = [0.0f32; 3];
            p[axis] = plane;
            p[u] = at(u, i);
            p[w] = at(w, j);
            p
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Cells(HashMap<BlockPos, u32>, u32);

    impl BlockAccess for Cells {
        fn block(&self, x: i32, y: i32, z: i32) -> u32 {
            self.0.get(&[x, y, z]).copied().unwrap_or(self.1)
        }
        fn liquid(&self, _: i32, _: i32, _: i32) -> u32 {
            self.1
        }
    }

    fn world(stones: &[BlockPos]) -> (Cells, &'static BlockRegistry) {
        let r = BlockRegistry::vanilla();
        let stone = r.find("minecraft:stone", "").unwrap();
        (Cells(stones.iter().map(|&p| (p, stone)).collect(), r.air_id()), r)
    }

    #[test]
    fn the_wall_that_hides_a_block_is_named() {
        // t101: the bot must be able to say *what* to dig first, not just that something is in the way.
        let (w, registry) = world(&[[2, 0, 0], [3, 0, 0], [6, 0, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 0.5, 0.5];
        assert_eq!(sight.occluder(eye, [3, 0, 0]), Some([2, 0, 0]), "the near wall, not the far stone");
        assert!(sight.visible_points(eye, [3, 0, 0]).is_empty());
        // Nothing in the way: no name to blame (the message keeps its old wording).
        assert_eq!(sight.occluder(eye, [2, 0, 0]), None);
        assert_eq!(sight.occluder(eye, [1, 0, 0]), None, "air is not an occluder");
        // The nearest obstacle wins when two are stacked in the ray.
        let (w, registry) = world(&[[2, 0, 0], [4, 0, 0], [5, 0, 0]]);
        let sight = Sight { world: &w, registry };
        assert_eq!(sight.occluder(eye, [5, 0, 0]), Some([2, 0, 0]));
    }

    #[test]
    fn a_wall_hides_the_block_behind_it() {
        let (w, registry) = world(&[[2, 0, 0], [3, 0, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 0.5, 0.5];
        assert!(sight.visible_points(eye, [3, 0, 0]).is_empty());
        let front = sight.visible_points(eye, [2, 0, 0]);
        assert!(!front.is_empty() && front.iter().all(|&(f, _)| f == Face::West));
        assert!(sight.sees_face_at(eye, [2, 0, 0], Face::West, [2.0, 0.5, 0.5]));
        assert!(!sight.sees_face_at(eye, [3, 0, 0], Face::West, [3.0, 0.5, 0.5]));
    }

    #[test]
    fn a_block_seen_from_above_shows_its_top_and_side() {
        let (w, registry) = world(&[[2, 0, 0]]);
        let sight = Sight { world: &w, registry };
        let faces: Vec<Face> = sight.visible_points([0.5, 2.6, 0.5], [2, 0, 0]).into_iter().map(|(f, _)| f).collect();
        assert!(faces.contains(&Face::Up) && faces.contains(&Face::West), "{faces:?}");
    }

    #[test]
    fn a_flower_hides_what_is_behind_it_and_can_be_aimed_at() {
        let r = BlockRegistry::vanilla();
        let (poppy, stone) = (r.find("minecraft:poppy", "").unwrap(), r.find("minecraft:stone", "").unwrap());
        let w = Cells([([2, 0, 0], poppy), ([3, 0, 0], stone)].into_iter().collect(), r.air_id());
        let sight = Sight { world: &w, registry: r };
        let eye = [0.5, 0.3, 0.45];
        assert!(!sight.clear(eye, [3.0, 0.3, 0.45]), "no collision, but the crosshair stops on it");
        assert!(!sight.visible_points(eye, [2, 0, 0]).is_empty());
    }

    #[test]
    fn an_entity_point_behind_a_wall_is_not_clear() {
        let (w, registry) = world(&[[2, 1, 0]]);
        let sight = Sight { world: &w, registry };
        assert!(!sight.clear([0.5, 1.5, 0.5], [3.5, 1.5, 0.5]));
        assert!(sight.clear([0.5, 1.5, 0.5], [1.9, 1.5, 0.5]));
    }

    #[test]
    fn a_block_below_the_feet_is_visible_from_the_eye() {
        // Standing on [0, -1, 0]; the cell at [0, -2, 0] is under the floor and must stay hidden, the one
        // beside it at [1, -1, 0] has its top exposed.
        let (w, registry) = world(&[[0, -1, 0], [0, -2, 0], [1, -1, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 1.62, 0.5];
        assert!(!sight.visible_points(eye, [1, -1, 0]).is_empty(), "the floor block beside the feet shows its top");
        assert!(sight.visible_points(eye, [0, -2, 0]).is_empty(), "the block under the floor is enclosed");
        // The straight-down cell is reached, and the face aimed at is Up.
        let under = sight.visible_points(eye, [0, -1, 0]);
        assert!(under.iter().any(|&(f, _)| f == Face::Up), "{under:?}");
    }

    #[test]
    fn a_block_at_the_same_level_in_front_is_visible_through_its_side() {
        let (w, registry) = world(&[[2, 0, 0], [0, -1, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 1.62, 0.5];
        let faces: Vec<Face> = sight.visible_points(eye, [2, 0, 0]).into_iter().map(|(f, _)| f).collect();
        assert!(faces.contains(&Face::West), "the side facing the bot is visible: {faces:?}");
        assert!(!sight.visible_points(eye, [2, 0, 0]).is_empty());
    }

    #[test]
    fn a_block_below_and_to_the_side_is_visible_from_its_top() {
        // Feet at y=0 over a floor at y=-1; the target at [1, -1, 0] is one down and one across: its top face
        // (the one above the floor) is exposed, and the eye looks down on it.
        let (w, registry) = world(&[[0, -1, 0], [1, -1, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 1.62, 0.5];
        let faces: Vec<Face> = sight.visible_points(eye, [1, -1, 0]).into_iter().map(|(f, _)| f).collect();
        assert!(faces.contains(&Face::Up), "{faces:?}");
        // An enclosing ceiling hides the top: then only the side that faces the bot's own column is left,
        // and that one is covered by the floor — nothing is visible and the caller must dig its way in.
        let (roofed, registry) = world(&[[0, -1, 0], [1, -1, 0], [1, 0, 0]]);
        let sight = Sight { world: &roofed, registry };
        assert!(sight.visible_points(eye, [1, -1, 0]).is_empty(), "no exposed face is left");
    }

    #[test]
    fn a_sliver_of_the_top_face_above_a_wall_still_counts() {
        // A fence-high shape next to the target hides most of its top; the probe grid must still find the gap.
        let (w, registry) = world(&[[2, 0, 0], [1, 0, 0]]);
        let sight = Sight { world: &w, registry };
        let eye = [0.5, 1.62, 0.5];
        assert!(!sight.visible_points(eye, [2, 0, 0]).is_empty());
    }
}
