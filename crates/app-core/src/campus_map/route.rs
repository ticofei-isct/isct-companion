//! Indoor routes, derived from the floor plans. The export has no doors and
//! no walk network, so one is inferred per building: walkable spaces
//! (corridors, lift lobbies, stair halls, porches and unnamed spaces) are
//! joined where their outlines run side by side long enough to hold a door,
//! a room joins the walkable space it borders, stair halls join the stair hall
//! above them, lift points stacked over each other form a shaft, and every
//! entrance joins the walkable space at it. A route is the cheapest path over
//! that network (A*), so it is a suggestion: it can pass a wall the plans do
//! not open. Shells say so.
//!
//! Everything here is in flat metres around the building's centre.
use super::{
    export::{AmenityCategory, Position, UnitCategory},
    geometry,
    map::Map,
};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
};

/// Two outlines this far apart still touch: the wall between them.
const WALL_M: f64 = 0.6;
/// The shortest shared side that can hold a door.
const DOOR_M: f64 = 0.8;
/// Sides at most this far from parallel count as one line (about 10°).
const PARALLEL_SINE: f64 = 0.17;
/// What one floor of stairs costs, in metres walked.
const STAIRS_M: f64 = 12.0;
/// What a lift ride costs (the wait), and each floor it passes.
const LIFT_M: f64 = 25.0;
const LIFT_FLOOR_M: f64 = 3.0;
/// Lift points this close on two floors are one shaft; stair points one
/// floor apart, one flight.
const SHAFT_M: f64 = 3.0;
const FLIGHT_M: f64 = 4.0;
/// A lift, stair or entrance point joins walkable space this near.
const REACH_M: f64 = 8.0;
/// Crossing an unnamed space costs this much more than a corridor: the plans
/// do not say it is open to everyone.
const UNNAMED_FACTOR: f64 = 1.3;
/// Steps along a straight line inside a space, to check it stays inside.
const SAMPLE_M: f64 = 0.5;

/// How a route goes up or down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Via {
    Stairs,
    Lift,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// A space's own point: a room's is where a route ends.
    Hub,
    Door,
    Shaft,
    Entrance(usize),
}

#[derive(Clone, Copy, Debug)]
struct Node {
    at: Position,
    xy: (f64, f64),
    level: usize,
    kind: Kind,
}

#[derive(Clone, Copy, Debug)]
struct Edge {
    to: usize,
    cost: f64,
    via: Option<Via>,
}

/// One building's walk network.
pub(super) struct Walk {
    nodes: Vec<Node>,
    edges: Vec<Vec<Edge>>,
    /// Unit → its point.
    hubs: HashMap<usize, usize>,
    /// Points of rooms: a route may end there, never pass through.
    rooms: HashSet<usize>,
    /// Entrance index in the building → its point.
    entrances: HashMap<usize, usize>,
}

/// One stretch of a route on one floor.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Leg {
    pub(super) level: usize,
    pub(super) points: Vec<Position>,
}

/// A change of floor: where, how, and between which floors.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Climb {
    pub(super) at: Position,
    pub(super) via: Via,
    pub(super) from: usize,
    pub(super) to: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Route {
    pub(super) entrance: usize,
    pub(super) length_m: f64,
    pub(super) legs: Vec<Leg>,
    pub(super) climbs: Vec<Climb>,
}

/// A space as the network sees it.
struct Space {
    unit: usize,
    level: usize,
    rings: Vec<Vec<(f64, f64)>>,
    bounds: [f64; 4],
    center: (f64, f64),
    walkable: bool,
    unnamed: bool,
}

fn walkable(category: UnitCategory) -> bool {
    matches!(
        category,
        UnitCategory::Corridor
            | UnitCategory::ElevatorLobby
            | UnitCategory::Stairs
            | UnitCategory::Lounge
            | UnitCategory::Porch
    )
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (b.0 - a.0).hypot(b.1 - a.1)
}

fn ring_contains(ring: &[(f64, f64)], p: (f64, f64)) -> bool {
    let mut inside = false;
    for pair in ring.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if (a.1 > p.1) != (b.1 > p.1) {
            let x = a.0 + (p.1 - a.1) * (b.0 - a.0) / (b.1 - a.1);
            if x > p.0 {
                inside = !inside;
            }
        }
    }
    inside
}

fn contains(rings: &[Vec<(f64, f64)>], p: (f64, f64)) -> bool {
    let mut rings = rings.iter();
    rings.next().is_some_and(|outer| ring_contains(outer, p))
        && rings.all(|hole| !ring_contains(hole, p))
}

fn to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    distance(p, (a.0 + t * dx, a.1 + t * dy))
}

fn to_edges(rings: &[Vec<(f64, f64)>], p: (f64, f64)) -> f64 {
    rings
        .iter()
        .flat_map(|ring| ring.windows(2))
        .map(|pair| to_segment(p, pair[0], pair[1]))
        .fold(f64::INFINITY, f64::min)
}

/// How far `p` is from the space: zero inside it.
fn space_distance(space: &Space, p: (f64, f64)) -> f64 {
    if contains(&space.rings, p) {
        0.0
    } else {
        to_edges(&space.rings, p)
    }
}

/// Where two sides run along each other: the length they share and its
/// middle, when they are near parallel, near each other and share at least
/// [`DOOR_M`].
fn shared_side(e: [(f64, f64); 2], f: [(f64, f64); 2]) -> Option<(f64, (f64, f64))> {
    let (a, b) = (e[0], e[1]);
    let length = distance(a, b);
    let other = distance(f[0], f[1]);
    if length < 1e-6 || other < 1e-6 {
        return None;
    }
    let u = ((b.0 - a.0) / length, (b.1 - a.1) / length);
    let v = ((f[1].0 - f[0].0) / other, (f[1].1 - f[0].1) / other);
    if (u.0 * v.1 - u.1 * v.0).abs() > PARALLEL_SINE {
        return None;
    }
    let off = |p: (f64, f64)| ((p.0 - a.0) * u.1 - (p.1 - a.1) * u.0).abs();
    if off(f[0]) > WALL_M || off(f[1]) > WALL_M {
        return None;
    }
    let along = |p: (f64, f64)| (p.0 - a.0) * u.0 + (p.1 - a.1) * u.1;
    let (t0, t1) = (along(f[0]), along(f[1]));
    let low = t0.min(t1).max(0.0);
    let high = t0.max(t1).min(length);
    if high - low < DOOR_M {
        return None;
    }
    let middle = (low + high) / 2.0;
    Some((high - low, (a.0 + u.0 * middle, a.1 + u.1 * middle)))
}

/// The longest stretch the two spaces' outer outlines share.
fn door(a: &Space, b: &Space) -> Option<(f64, f64)> {
    let near = a.bounds[0] <= b.bounds[2] + WALL_M
        && b.bounds[0] <= a.bounds[2] + WALL_M
        && a.bounds[1] <= b.bounds[3] + WALL_M
        && b.bounds[1] <= a.bounds[3] + WALL_M;
    if !near {
        return None;
    }
    let (outer_a, outer_b) = (a.rings.first()?, b.rings.first()?);
    let mut best: Option<(f64, (f64, f64))> = None;
    for e in outer_a.windows(2) {
        for f in outer_b.windows(2) {
            if let Some(found) = shared_side([e[0], e[1]], [f[0], f[1]]) {
                if best.is_none_or(|(length, _)| found.0 > length) {
                    best = Some(found);
                }
            }
        }
    }
    best.map(|(_, at)| at)
}

/// Whether the straight line stays inside the space (or on its outline).
fn stays_inside(space: &Space, from: (f64, f64), to: (f64, f64)) -> bool {
    let steps = (distance(from, to) / SAMPLE_M).ceil().max(2.0) as usize;
    (1..steps).all(|step| {
        let t = step as f64 / steps as f64;
        let p = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        contains(&space.rings, p) || to_edges(&space.rings, p) <= 0.3
    })
}

impl Walk {
    pub(super) fn build(map: &Map, building: usize) -> Self {
        let record = &map.export.buildings[building];
        let origin = record.center;
        let flat = |p: Position| geometry::metres(origin, p);
        let ordinals: Vec<i32> = record.levels.iter().map(|level| level.ordinal).collect();
        let mut walk = Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            hubs: HashMap::new(),
            rooms: HashSet::new(),
            entrances: HashMap::new(),
        };
        let mut spaces: Vec<Space> = Vec::new();
        for (level, record_level) in record.levels.iter().enumerate() {
            for unit in map
                .units_by_level
                .get(&record_level.id)
                .into_iter()
                .flatten()
            {
                let source = &map.export.units[*unit];
                let unnamed = source.category == UnitCategory::Room
                    && source.room_id.is_none()
                    && source.label.is_none();
                let rings: Vec<Vec<(f64, f64)>> = source
                    .polygon
                    .iter()
                    .map(|ring| ring.iter().copied().map(flat).collect())
                    .collect();
                let Some(points) = rings.first().filter(|ring| ring.len() > 3) else {
                    continue;
                };
                let mut bounds = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                for p in points {
                    bounds = [
                        bounds[0].min(p.0),
                        bounds[1].min(p.1),
                        bounds[2].max(p.0),
                        bounds[3].max(p.1),
                    ];
                }
                spaces.push(Space {
                    unit: *unit,
                    level,
                    center: flat(source.center),
                    rings,
                    bounds,
                    walkable: walkable(source.category) || unnamed,
                    unnamed,
                });
            }
        }
        for space in &spaces {
            let source = &map.export.units[space.unit];
            let hub = walk.node(source.center, space.center, space.level, Kind::Hub);
            walk.hubs.insert(space.unit, hub);
            if !space.walkable {
                walk.rooms.insert(hub);
            }
        }
        // Doors between spaces of one floor, one side at least walkable.
        let mut doors: HashMap<usize, Vec<usize>> = HashMap::new();
        for (index, a) in spaces.iter().enumerate() {
            for b in &spaces[index + 1..] {
                if a.level != b.level || !(a.walkable || b.walkable) {
                    continue;
                }
                if let Some(xy) = door(a, b) {
                    let at = geometry::position(origin, xy);
                    let node = walk.node(at, xy, a.level, Kind::Door);
                    doors.entry(a.unit).or_default().push(node);
                    doors.entry(b.unit).or_default().push(node);
                }
            }
        }
        // Inside a space: its point to each door, and door to door when the
        // line stays inside a walkable space.
        for space in &spaces {
            let hub = walk.hubs[&space.unit];
            let factor = if space.unnamed { UNNAMED_FACTOR } else { 1.0 };
            let own = doors.get(&space.unit).cloned().unwrap_or_default();
            for door in &own {
                let cost = distance(walk.nodes[hub].xy, walk.nodes[*door].xy) * factor;
                walk.link(hub, *door, cost, None);
            }
            if space.walkable {
                for (index, a) in own.iter().enumerate() {
                    for b in &own[index + 1..] {
                        let (from, to) = (walk.nodes[*a].xy, walk.nodes[*b].xy);
                        if stays_inside(space, from, to) {
                            walk.link(*a, *b, distance(from, to) * factor, None);
                        }
                    }
                }
            }
        }
        // A point joins the walkable space holding it, else the nearest
        // within reach, at that space's own point.
        let attach = |walk: &mut Self, at: Position, level: usize, kind: Kind| -> Option<usize> {
            let xy = flat(at);
            let space = spaces
                .iter()
                .filter(|space| space.level == level && space.walkable)
                .map(|space| (space_distance(space, xy), space))
                .filter(|(gap, _)| *gap <= REACH_M)
                .min_by(|a, b| a.0.total_cmp(&b.0))?
                .1;
            let hub = walk.hubs[&space.unit];
            let node = walk.node(at, xy, level, kind);
            walk.link(node, hub, distance(xy, space.center), None);
            Some(node)
        };
        // Stair halls over each other, one floor apart.
        let stairs: Vec<&Space> = spaces
            .iter()
            .filter(|space| map.export.units[space.unit].category == UnitCategory::Stairs)
            .collect();
        for a in &stairs {
            for b in &stairs {
                if ordinals[b.level] != ordinals[a.level] + 1 {
                    continue;
                }
                if contains(&a.rings, b.center)
                    || contains(&b.rings, a.center)
                    || distance(a.center, b.center) < FLIGHT_M
                {
                    let (lower, upper) = (walk.hubs[&a.unit], walk.hubs[&b.unit]);
                    walk.link(lower, upper, STAIRS_M, Some(Via::Stairs));
                }
            }
        }
        // Lift and stair points stacked across floors.
        let mut lifts: Vec<(usize, usize)> = Vec::new();
        let mut flights: Vec<(usize, usize)> = Vec::new();
        for (level, record_level) in record.levels.iter().enumerate() {
            for amenity in map
                .amenities_by_level
                .get(&record_level.id)
                .into_iter()
                .flatten()
            {
                let source = &map.export.amenities[*amenity];
                let list = match source.category {
                    AmenityCategory::Elevator => &mut lifts,
                    AmenityCategory::Stairs => &mut flights,
                    _ => continue,
                };
                if let Some(node) = attach(&mut walk, source.point, level, Kind::Shaft) {
                    list.push((node, level));
                }
            }
        }
        for (index, (a, level_a)) in lifts.iter().enumerate() {
            for (b, level_b) in &lifts[index + 1..] {
                let floors = (ordinals[*level_a] - ordinals[*level_b]).unsigned_abs();
                if floors > 0 && distance(walk.nodes[*a].xy, walk.nodes[*b].xy) <= SHAFT_M {
                    walk.link(
                        *a,
                        *b,
                        LIFT_M + LIFT_FLOOR_M * f64::from(floors),
                        Some(Via::Lift),
                    );
                }
            }
        }
        for (index, (a, level_a)) in flights.iter().enumerate() {
            for (b, level_b) in &flights[index + 1..] {
                let floors = (ordinals[*level_a] - ordinals[*level_b]).unsigned_abs();
                if floors == 1 && distance(walk.nodes[*a].xy, walk.nodes[*b].xy) <= FLIGHT_M {
                    walk.link(*a, *b, STAIRS_M, Some(Via::Stairs));
                }
            }
        }
        for (index, entrance) in record.entrances.iter().enumerate() {
            let Some(level) = entrance
                .level_id
                .as_deref()
                .and_then(|id| record.levels.iter().position(|level| level.id == id))
            else {
                continue;
            };
            if let Some(node) = attach(&mut walk, entrance.point, level, Kind::Entrance(index)) {
                walk.entrances.insert(index, node);
            }
        }
        walk
    }

    fn node(&mut self, at: Position, xy: (f64, f64), level: usize, kind: Kind) -> usize {
        self.nodes.push(Node {
            at,
            xy,
            level,
            kind,
        });
        self.edges.push(Vec::new());
        self.nodes.len() - 1
    }

    fn link(&mut self, a: usize, b: usize, cost: f64, via: Option<Via>) {
        self.edges[a].push(Edge { to: b, cost, via });
        self.edges[b].push(Edge { to: a, cost, via });
    }

    /// Whether a route can start anywhere: some entrance joins the network.
    pub(super) fn has_entrances(&self) -> bool {
        !self.entrances.is_empty()
    }

    /// The cheapest route from an entrance (`from`, else the best of all)
    /// to any of `units`; `step_free` never takes stairs.
    pub(super) fn route(
        &self,
        from: Option<usize>,
        units: &[usize],
        step_free: bool,
    ) -> Option<Route> {
        let goals: HashSet<usize> = units
            .iter()
            .filter_map(|unit| self.hubs.get(unit))
            .copied()
            .collect();
        if goals.is_empty() {
            return None;
        }
        let starts: Vec<usize> = match from {
            Some(entrance) => self.entrances.get(&entrance).copied().into_iter().collect(),
            None => self.entrances.values().copied().collect(),
        };
        let goal_points: Vec<(f64, f64)> = goals.iter().map(|goal| self.nodes[*goal].xy).collect();
        let estimate = |node: usize| {
            goal_points
                .iter()
                .map(|goal| distance(self.nodes[node].xy, *goal))
                .fold(f64::INFINITY, f64::min)
        };
        let mut best: HashMap<usize, f64> = HashMap::new();
        let mut came: HashMap<usize, (usize, Option<Via>)> = HashMap::new();
        let mut open = BinaryHeap::new();
        for node in starts {
            best.insert(node, 0.0);
            open.push(Open {
                estimate: estimate(node),
                node,
            });
        }
        let mut reached = None;
        while let Some(Open { node, .. }) = open.pop() {
            if goals.contains(&node) {
                reached = Some(node);
                break;
            }
            let here = best.get(&node).copied().unwrap_or(f64::INFINITY);
            // A room is a destination, never a way through.
            if self.rooms.contains(&node) {
                continue;
            }
            for edge in &self.edges[node] {
                if step_free && edge.via == Some(Via::Stairs) {
                    continue;
                }
                let cost = here + edge.cost;
                if cost < best.get(&edge.to).copied().unwrap_or(f64::INFINITY) {
                    best.insert(edge.to, cost);
                    came.insert(edge.to, (node, edge.via));
                    open.push(Open {
                        estimate: cost + estimate(edge.to),
                        node: edge.to,
                    });
                }
            }
        }
        let end = reached?;
        // Back from the end along the cheapest way in to each point.
        let mut path = vec![end];
        let mut at = end;
        while let Some((previous, _)) = came.get(&at).copied() {
            path.push(previous);
            at = previous;
        }
        path.reverse();
        let Kind::Entrance(entrance) = self.nodes[*path.first()?].kind else {
            return None;
        };
        let mut legs: Vec<Leg> = Vec::new();
        let mut climbs: Vec<Climb> = Vec::new();
        for pair in path.windows(2) {
            let (before, after) = (self.nodes[pair[0]], self.nodes[pair[1]]);
            if before.level != after.level {
                // The way into `after` is the edge that changed floors.
                let via = came
                    .get(&pair[1])
                    .and_then(|(_, via)| *via)
                    .unwrap_or(Via::Stairs);
                climbs.push(Climb {
                    at: before.at,
                    via,
                    from: before.level,
                    to: after.level,
                });
            }
        }
        for node in &path {
            let record = self.nodes[*node];
            match legs.last_mut() {
                Some(leg) if leg.level == record.level => leg.points.push(record.at),
                _ => legs.push(Leg {
                    level: record.level,
                    points: vec![record.at],
                }),
            }
        }
        Some(Route {
            entrance,
            length_m: best.get(&end).copied().unwrap_or(0.0),
            legs,
            climbs,
        })
    }
}

/// An open point of the search, cheapest estimate first.
struct Open {
    estimate: f64,
    node: usize,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.estimate.total_cmp(&other.estimate) == Ordering::Equal
    }
}

impl Eq for Open {}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.estimate.total_cmp(&self.estimate)
    }
}

#[cfg(test)]
#[path = "../tests/campus_map__route.rs"]
mod tests;
