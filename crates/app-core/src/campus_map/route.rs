//! Walking routes: over the campus walkways to a building's entrance, then
//! through the building to a room. Nothing here asks a service: the walkways
//! are a bundled OpenStreetMap extract (`walkways.txt`), and the way through
//! a building is worked out from its floor plans.
//!
//! Indoors each floor is cut into a grid of [`CELL_M`] cells. A cell is
//! walkable when its centre is in a corridor, lobby, stair hall, lounge or
//! porch of the plan and in no room; the space of a floor that the plan
//! draws no unit for is never walked (it is as often a courtyard or a void
//! as a hall). The plans draw doors as closed lines, so a wall line inside
//! walkable space is not a barrier but a cost ([`WALL_M`]): a route goes
//! round a drawn box when that is near, and through a drawn door when it is
//! the way. Rooms are destinations, never passages; the plans have no doors,
//! so a route reaches a room where the room meets walkable space. Floors
//! join at the stairs and lifts the plans mark, where the same shaft is
//! marked on both. A floor with no corridors drawn is crossed blind, from
//! its entrance straight to a stair or lift.
use super::{
    export::{AmenityCategory, LineTone, Position, Rings, UnitCategory},
    geometry,
    map::Map,
};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    sync::OnceLock,
};

/// The side of a grid cell, metres.
const CELL_M: f64 = 0.25;
/// The grid reaches this far past a building's plans.
const PAD_M: f64 = 3.0;
/// Crossing a wall line drawn inside walkable space costs this many metres.
const WALL_M: f32 = 8.0;
/// A room meets walkable space across a gap of this many cells (a wall's
/// thickness on the plans).
const ROOM_REACH: usize = 3;
/// An entrance joins walkable space this near, a stair or lift mark this
/// near.
const ENTRANCE_REACH_M: f64 = 8.0;
const CONNECTOR_REACH_M: f64 = 4.0;
/// Stair or lift marks on two floors this close are one shaft.
const SHAFT_REACH_M: f64 = 3.0;
/// A floor by stairs, a lift ride and each floor it passes, in metres of
/// walking.
const STAIR_M: f32 = 15.0;
const LIFT_M: f32 = 30.0;
const LIFT_FLOOR_M: f32 = 2.0;
/// A blind crossing is taken to be this much longer than its straight line.
const BLIND: f64 = 1.3;
/// An entrance joins the walkways within this distance, the person's
/// position within that one.
const ENTRANCE_SNAP_M: f64 = 40.0;
const ORIGIN_SNAP_M: f64 = 150.0;

/// A polygon's rings in a building's own metres.
type Shape = Vec<Vec<(f64, f64)>>;

const WALK: u8 = 1;
const WALL: u8 = 2;
const STAIRS: u8 = 4;

/// The next node to settle, nearest first.
#[derive(PartialEq)]
struct Visit {
    cost: f32,
    node: u32,
}

impl Eq for Visit {}

impl Ord for Visit {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Visit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// MARK: Outdoors

/// The campus walkways as a graph, lengths in metres.
#[derive(Clone)]
pub(super) struct Walkways {
    nodes: Vec<Position>,
    links: Vec<Vec<(u32, f32)>>,
}

impl Walkways {
    pub(super) fn bundled() -> Self {
        Self::parse(include_str!("walkways.txt"))
    }

    /// Reads `n <lng> <lat>` (1e-7 degrees) and `e <node> <node> <kind>`
    /// lines; anything else is skipped.
    fn parse(text: &str) -> Self {
        let mut walkways = Self {
            nodes: Vec::new(),
            links: Vec::new(),
        };
        for line in text.lines() {
            let mut fields = line.split_ascii_whitespace();
            match fields.next() {
                Some("n") => {
                    let mut degrees = || {
                        fields
                            .next()
                            .and_then(|field| field.parse::<i64>().ok())
                            .map(|value| value as f64 / 1e7)
                    };
                    if let (Some(lng), Some(lat)) = (degrees(), degrees()) {
                        walkways.nodes.push([lng, lat]);
                        walkways.links.push(Vec::new());
                    }
                }
                Some("e") => {
                    let mut node = || {
                        fields
                            .next()
                            .and_then(|field| field.parse::<u32>().ok())
                            .filter(|node| (*node as usize) < walkways.nodes.len())
                    };
                    if let (Some(a), Some(b)) = (node(), node()) {
                        walkways.link(a, b);
                    }
                }
                _ => {}
            }
        }
        walkways
    }

    fn link(&mut self, a: u32, b: u32) {
        let metres = distance(self.nodes[a as usize], self.nodes[b as usize]) as f32;
        self.links[a as usize].push((b, metres));
        self.links[b as usize].push((a, metres));
    }

    fn node(&mut self, at: Position) -> u32 {
        self.nodes.push(at);
        self.links.push(Vec::new());
        (self.nodes.len() - 1) as u32
    }

    /// Joins `point` to the nearest walkway within `reach` metres, at the
    /// point of it nearest: the node made for `point`.
    fn join(&mut self, point: Position, reach: f64) -> Option<u32> {
        let mut best: Option<(f64, u32, u32, f64)> = None;
        for (a, links) in self.links.iter().enumerate() {
            let (ax, ay) = geometry::metres(point, self.nodes[a]);
            for (b, _) in links {
                if (*b as usize) < a {
                    continue;
                }
                let (bx, by) = geometry::metres(point, self.nodes[*b as usize]);
                let (dx, dy) = (bx - ax, by - ay);
                let length = dx * dx + dy * dy;
                let along = if length == 0.0 {
                    0.0
                } else {
                    (-(ax * dx + ay * dy) / length).clamp(0.0, 1.0)
                };
                let off = (ax + along * dx).hypot(ay + along * dy);
                if best.is_none_or(|(nearest, ..)| off < nearest) {
                    best = Some((off, a as u32, *b, along));
                }
            }
        }
        let (off, a, b, along) = best.filter(|(off, ..)| *off <= reach)?;
        let (from, to) = (self.nodes[a as usize], self.nodes[b as usize]);
        let foot = self.node([
            from[0] + (to[0] - from[0]) * along,
            from[1] + (to[1] - from[1]) * along,
        ]);
        self.link(foot, a);
        self.link(foot, b);
        let joined = self.node(point);
        self.links[joined as usize].push((foot, off as f32));
        self.links[foot as usize].push((joined, off as f32));
        Some(joined)
    }

    /// The shortest way from `from` to every node: its length and the node
    /// before it.
    fn spread(&self, from: u32) -> (Vec<f32>, Vec<u32>) {
        let mut cost = vec![f32::INFINITY; self.nodes.len()];
        let mut before = vec![u32::MAX; self.nodes.len()];
        let mut queue = BinaryHeap::new();
        cost[from as usize] = 0.0;
        queue.push(Visit {
            cost: 0.0,
            node: from,
        });
        while let Some(Visit { cost: so_far, node }) = queue.pop() {
            if so_far > cost[node as usize] {
                continue;
            }
            for (next, metres) in &self.links[node as usize] {
                let reached = so_far + metres;
                if reached < cost[*next as usize] {
                    cost[*next as usize] = reached;
                    before[*next as usize] = node;
                    queue.push(Visit {
                        cost: reached,
                        node: *next,
                    });
                }
            }
        }
        (cost, before)
    }
}

fn distance(a: Position, b: Position) -> f64 {
    let (east, north) = geometry::metres(a, b);
    east.hypot(north)
}

// MARK: Indoors

/// How two floors join.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Shaft {
    Stairs,
    Lift,
}

/// A stair or lift mark on a floor, and the walkable cell it is reached
/// from.
struct Connector {
    floor: usize,
    shaft: Shaft,
    at: (f64, f64),
    cell: Option<usize>,
}

/// A building entrance on a floor with plans.
struct Door {
    entrance: usize,
    floor: usize,
    at: (f64, f64),
    /// The walkable cell it opens on, and how far that is.
    cell: Option<(usize, f32)>,
}

struct Floor {
    /// Index in the building's levels.
    level: usize,
    ordinal: i32,
    /// `WALK | WALL | STAIRS` per cell; empty for a floor with nothing
    /// walkable drawn.
    cells: Vec<u8>,
    /// Cells to the nearest wall or edge of walkable space, at most 3; 0
    /// for a cell that is one.
    clear: Vec<u8>,
}

/// One building's floors as a graph: grid cells, connectors and doors.
pub(super) struct Indoor {
    building: usize,
    center: Position,
    west: f64,
    south: f64,
    width: usize,
    height: usize,
    floors: Vec<Floor>,
    connectors: Vec<Connector>,
    /// Shaft links between connectors, in metres of walking.
    shafts: Vec<Vec<(usize, f32)>>,
    doors: Vec<Door>,
    /// Blind crossings: door index and connector index, with their cost.
    blind: Vec<(usize, usize, f32)>,
    by_cell: HashMap<u32, Vec<Stop>>,
}

/// What a cell is the way to.
#[derive(Clone, Copy)]
enum Stop {
    Connector(usize),
    Door(usize),
}

/// A stretch of a route on one floor.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Leg {
    /// Index in the building's levels.
    pub(super) level: usize,
    pub(super) points: Vec<Position>,
    /// Across a floor with no corridors drawn: a straight guess.
    pub(super) blind: bool,
    /// How the next floor is reached from this leg's end.
    pub(super) leaves_by: Option<Shaft>,
}

/// The way from every door of a building to one unit.
pub(super) struct Reach {
    cost: HashMap<u32, f32>,
    /// The node after each on the way to the unit.
    toward: HashMap<u32, u32>,
    unit: usize,
}

impl Indoor {
    /// The grid of `building`; `None` when its plans draw nothing walkable.
    pub(super) fn new(map: &Map, building: usize) -> Option<Self> {
        let record = &map.export.buildings[building];
        let center = record.center;
        let local = |point: Position| geometry::metres(center, point);
        let mut bounds: Option<[f64; 4]> = None;
        let mut widen = |point: Position| {
            let (x, y) = local(point);
            let b = bounds.get_or_insert([x, y, x, y]);
            *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        };
        for level in &record.levels {
            level
                .outline
                .iter()
                .flatten()
                .flatten()
                .copied()
                .for_each(&mut widen);
            for unit in map.units_by_level.get(&level.id).into_iter().flatten() {
                map.export.units[*unit]
                    .polygon
                    .iter()
                    .flatten()
                    .copied()
                    .for_each(&mut widen);
            }
        }
        let [west, south, east, north] = bounds?;
        let (west, south) = (west - PAD_M, south - PAD_M);
        let width = ((east + PAD_M - west) / CELL_M).ceil() as usize;
        let height = ((north + PAD_M - south) / CELL_M).ceil() as usize;
        let mut order: Vec<usize> = (0..record.levels.len()).collect();
        order.sort_by_key(|level| record.levels[*level].ordinal);
        let mut indoor = Self {
            building,
            center,
            west,
            south,
            width,
            height,
            floors: Vec::new(),
            connectors: Vec::new(),
            shafts: Vec::new(),
            doors: Vec::new(),
            blind: Vec::new(),
            by_cell: HashMap::new(),
        };
        for level in order {
            let floor = indoor.floor(map, level);
            indoor.floors.push(floor);
        }
        if indoor.floors.iter().all(|floor| floor.cells.is_empty()) {
            return None;
        }
        indoor.connect(map);
        Some(indoor)
    }

    fn local(&self, point: Position) -> (f64, f64) {
        geometry::metres(self.center, point)
    }

    fn rings(&self, rings: &Rings) -> Shape {
        rings
            .iter()
            .map(|ring| ring.iter().map(|point| self.local(*point)).collect())
            .collect()
    }

    fn centre(&self, cell: usize) -> (f64, f64) {
        (
            self.west + ((cell % self.width) as f64 + 0.5) * CELL_M,
            self.south + ((cell / self.width) as f64 + 0.5) * CELL_M,
        )
    }

    fn cell_at(&self, at: (f64, f64)) -> Option<usize> {
        let column = ((at.0 - self.west) / CELL_M).floor();
        let row = ((at.1 - self.south) / CELL_M).floor();
        (column >= 0.0
            && row >= 0.0
            && (column as usize) < self.width
            && (row as usize) < self.height)
            .then(|| row as usize * self.width + column as usize)
    }

    /// Calls `mark` with each run of cells whose centres are inside the
    /// rings (even-odd, so holes are out): a row and its first and
    /// one-past-last column.
    fn fill(&self, rings: &[Vec<(f64, f64)>], mut mark: impl FnMut(usize, usize, usize)) {
        let mut crossings = Vec::new();
        for row in 0..self.height {
            let y = self.south + (row as f64 + 0.5) * CELL_M;
            crossings.clear();
            for ring in rings {
                for (index, a) in ring.iter().enumerate() {
                    let b = ring[(index + 1) % ring.len()];
                    if (a.1 > y) != (b.1 > y) {
                        crossings.push(a.0 + (y - a.1) * (b.0 - a.0) / (b.1 - a.1));
                    }
                }
            }
            crossings.sort_by(f64::total_cmp);
            for pair in crossings.chunks_exact(2) {
                let column = |x: f64| ((x - self.west) / CELL_M - 0.5).ceil().max(0.0) as usize;
                let (first, last) = (column(pair[0]), column(pair[1]).min(self.width));
                if last > first {
                    mark(row, first, last);
                }
            }
        }
    }

    /// One floor's grid from its plan.
    fn floor(&self, map: &Map, level: usize) -> Floor {
        let source = &map.export.buildings[self.building].levels[level];
        let mut floor = Floor {
            level,
            ordinal: source.ordinal,
            cells: Vec::new(),
            clear: Vec::new(),
        };
        let units: Vec<usize> = map
            .units_by_level
            .get(&source.id)
            .cloned()
            .unwrap_or_default();
        if !units
            .iter()
            .any(|unit| walkable(map.export.units[*unit].category))
        {
            return floor;
        }
        let size = self.width * self.height;
        let mut cells = vec![0u8; size];
        // Largest first, so a room drawn inside a corridor's polygon cuts
        // its own shape out of it.
        let shapes: Vec<(usize, Shape)> = units
            .iter()
            .map(|unit| (*unit, self.rings(&map.export.units[*unit].polygon)))
            .collect();
        let mut by_area: Vec<usize> = (0..shapes.len()).collect();
        by_area.sort_by(|a, b| area(&shapes[*b].1).total_cmp(&area(&shapes[*a].1)));
        for index in by_area {
            let (unit, rings) = &shapes[index];
            let category = map.export.units[*unit].category;
            self.fill(rings, |row, first, last| {
                for cell in &mut cells[row * self.width + first..row * self.width + last] {
                    if walkable(category) {
                        *cell |= WALK;
                        if category == UnitCategory::Stairs {
                            *cell |= STAIRS;
                        }
                    } else {
                        *cell &= !WALK;
                    }
                }
            });
        }
        // Wall lines, every cell they pass through; the treads drawn in a
        // stair hall are not walls.
        for index in map.linework_by_level.get(&source.id).into_iter().flatten() {
            let linework = &map.export.linework[*index];
            if linework.tone != LineTone::Wall {
                continue;
            }
            for line in &linework.lines {
                for pair in line.windows(2) {
                    let (a, b) = (self.local(pair[0]), self.local(pair[1]));
                    let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) / (CELL_M / 4.0)).ceil()
                        as usize
                        + 1;
                    for step in 0..=steps {
                        let along = step as f64 / steps as f64;
                        let at = (a.0 + (b.0 - a.0) * along, a.1 + (b.1 - a.1) * along);
                        if let Some(cell) = self.cell_at(at) {
                            if cells[cell] & STAIRS == 0 {
                                cells[cell] |= WALL;
                            }
                        }
                    }
                }
            }
        }
        // How far each free cell is from a wall or the edge, to three cells.
        let free = |cells: &[u8], cell: usize| cells[cell] & (WALK | WALL) == WALK;
        let mut clear: Vec<u8> = (0..size).map(|cell| u8::from(free(&cells, cell))).collect();
        for depth in 1..3u8 {
            let before = clear.clone();
            for row in 0..self.height {
                for column in 0..self.width {
                    let cell = row * self.width + column;
                    if before[cell] < depth {
                        continue;
                    }
                    let inner = row > 0
                        && column > 0
                        && row + 1 < self.height
                        && column + 1 < self.width
                        && (0..9).all(|k| {
                            before[(row + k / 3 - 1) * self.width + column + k % 3 - 1] >= depth
                        });
                    if inner {
                        clear[cell] = depth + 1;
                    }
                }
            }
        }
        floor.cells = cells;
        floor.clear = clear;
        floor
    }

    /// The nearest free cell to `at` within `reach` metres.
    fn attach(&self, floor: &Floor, at: (f64, f64), reach: f64) -> Option<(usize, f32)> {
        if floor.cells.is_empty() {
            return None;
        }
        let span = (reach / CELL_M) as i64 + 1;
        let column = ((at.0 - self.west) / CELL_M).floor() as i64;
        let row = ((at.1 - self.south) / CELL_M).floor() as i64;
        let mut best: Option<(usize, f64)> = None;
        for j in (row - span).max(0)..(row + span + 1).min(self.height as i64) {
            for i in (column - span).max(0)..(column + span + 1).min(self.width as i64) {
                let cell = j as usize * self.width + i as usize;
                if floor.cells[cell] & (WALK | WALL) != WALK {
                    continue;
                }
                let centre = self.centre(cell);
                let off = (centre.0 - at.0).hypot(centre.1 - at.1);
                if off <= reach && best.is_none_or(|(_, nearest)| off < nearest) {
                    best = Some((cell, off));
                }
            }
        }
        best.map(|(cell, off)| (cell, off as f32))
    }

    /// Stairs, lifts and doors, and what joins them.
    fn connect(&mut self, map: &Map) {
        let record = &map.export.buildings[self.building];
        for floor in 0..self.floors.len() {
            let source = &record.levels[self.floors[floor].level];
            let mut marks: Vec<(Shaft, Position)> = Vec::new();
            for amenity in map.amenities_by_level.get(&source.id).into_iter().flatten() {
                let amenity = &map.export.amenities[*amenity];
                match amenity.category {
                    AmenityCategory::Stairs => marks.push((Shaft::Stairs, amenity.point)),
                    AmenityCategory::Elevator => marks.push((Shaft::Lift, amenity.point)),
                    _ => {}
                }
            }
            // A stair hall the plan puts no stair mark in is one itself.
            for unit in map.units_by_level.get(&source.id).into_iter().flatten() {
                let unit = &map.export.units[*unit];
                if unit.category == UnitCategory::Stairs
                    && !marks.iter().any(|(shaft, at)| {
                        *shaft == Shaft::Stairs && geometry::polygon_contains(&unit.polygon, *at)
                    })
                {
                    marks.push((Shaft::Stairs, unit.center));
                }
            }
            for (shaft, point) in marks {
                let at = self.local(point);
                let cell = self
                    .attach(&self.floors[floor], at, CONNECTOR_REACH_M)
                    .map(|(cell, _)| cell);
                self.connectors.push(Connector {
                    floor,
                    shaft,
                    at,
                    cell,
                });
            }
        }
        // Each mark joins the same shaft on the nearest floor above.
        self.shafts = vec![Vec::new(); self.connectors.len()];
        for lower in 0..self.connectors.len() {
            let from = &self.connectors[lower];
            let upper = (0..self.connectors.len())
                .filter(|upper| {
                    let to = &self.connectors[*upper];
                    to.shaft == from.shaft
                        && to.floor > from.floor
                        && (to.at.0 - from.at.0).hypot(to.at.1 - from.at.1) <= SHAFT_REACH_M
                })
                .min_by_key(|upper| self.connectors[*upper].floor);
            if let Some(upper) = upper {
                let floors = (self.floors[self.connectors[upper].floor].ordinal
                    - self.floors[from.floor].ordinal) as f32;
                let cost = match from.shaft {
                    Shaft::Stairs => STAIR_M * floors,
                    Shaft::Lift => LIFT_M + LIFT_FLOOR_M * floors,
                };
                self.shafts[lower].push((upper, cost));
                self.shafts[upper].push((lower, cost));
            }
        }
        for (entrance, source) in record.entrances.iter().enumerate() {
            let Some(floor) = self.floors.iter().position(|floor| {
                source.level_id.as_deref() == Some(record.levels[floor.level].id.as_str())
            }) else {
                continue;
            };
            let at = self.local(source.point);
            let cell = self.attach(&self.floors[floor], at, ENTRANCE_REACH_M);
            self.doors.push(Door {
                entrance,
                floor,
                at,
                cell,
            });
        }
        let mut blind = Vec::new();
        for (door, entry) in self.doors.iter().enumerate() {
            if !self.floors[entry.floor].cells.is_empty() {
                continue;
            }
            for (connector, mark) in self.connectors.iter().enumerate() {
                if mark.floor == entry.floor {
                    let line = (mark.at.0 - entry.at.0).hypot(mark.at.1 - entry.at.1);
                    blind.push((door, connector, (BLIND * line) as f32));
                }
            }
        }
        self.blind = blind;
        let mut by_cell: HashMap<u32, Vec<Stop>> = HashMap::new();
        for (index, connector) in self.connectors.iter().enumerate() {
            if let Some(cell) = connector.cell {
                let node = self.cell_node(connector.floor, cell);
                by_cell
                    .entry(node)
                    .or_default()
                    .push(Stop::Connector(index));
            }
        }
        for (index, door) in self.doors.iter().enumerate() {
            if let Some((cell, _)) = door.cell {
                let node = self.cell_node(door.floor, cell);
                by_cell.entry(node).or_default().push(Stop::Door(index));
            }
        }
        self.by_cell = by_cell;
    }

    // Nodes: every floor's cells, then the connectors, then the doors.

    fn cell_node(&self, floor: usize, cell: usize) -> u32 {
        (floor * self.width * self.height + cell) as u32
    }

    fn connector_node(&self, connector: usize) -> u32 {
        (self.floors.len() * self.width * self.height + connector) as u32
    }

    fn door_node(&self, door: usize) -> u32 {
        self.connector_node(self.connectors.len() + door)
    }

    fn stop(&self, node: u32) -> Result<(usize, usize), Stop> {
        let cells = self.floors.len() * self.width * self.height;
        let node = node as usize;
        if node < cells {
            Ok((
                node / (self.width * self.height),
                node % (self.width * self.height),
            ))
        } else if node - cells < self.connectors.len() {
            Err(Stop::Connector(node - cells))
        } else {
            Err(Stop::Door(node - cells - self.connectors.len()))
        }
    }

    /// Every step from `node`, with its cost in metres.
    fn steps(&self, node: u32, mut step: impl FnMut(u32, f32)) {
        match self.stop(node) {
            Ok((floor, cell)) => {
                let grid = &self.floors[floor];
                let (row, column) = (cell / self.width, cell % self.width);
                for k in 0..9usize {
                    if k == 4 {
                        continue;
                    }
                    let (dy, dx) = (k / 3, k % 3);
                    if row + dy < 1
                        || column + dx < 1
                        || row + dy > self.height
                        || column + dx > self.width
                    {
                        continue;
                    }
                    let (to_row, to_column) = (row + dy - 1, column + dx - 1);
                    let to = to_row * self.width + to_column;
                    if grid.cells[to] & WALK == 0 {
                        continue;
                    }
                    let diagonal = dy != 1 && dx != 1;
                    // No cutting a corner between two cells that are not
                    // both walkable.
                    if diagonal
                        && (grid.cells[row * self.width + to_column] & WALK == 0
                            || grid.cells[to_row * self.width + column] & WALK == 0)
                    {
                        continue;
                    }
                    let length = CELL_M as f32
                        * if diagonal {
                            std::f32::consts::SQRT_2
                        } else {
                            1.0
                        };
                    // Keeping off the walls is a little shorter than
                    // brushing them.
                    let mut cost = length * (1.0 + 0.5 * f32::from(3 - grid.clear[to]) / 3.0);
                    if grid.cells[to] & WALL != 0 && grid.cells[cell] & WALL == 0 {
                        cost += WALL_M;
                    }
                    step(self.cell_node(floor, to), cost);
                }
                for stop in self.by_cell.get(&node).into_iter().flatten() {
                    let centre = self.centre(cell);
                    match stop {
                        Stop::Connector(index) => {
                            let at = self.connectors[*index].at;
                            step(
                                self.connector_node(*index),
                                (centre.0 - at.0).hypot(centre.1 - at.1) as f32,
                            );
                        }
                        Stop::Door(index) => {
                            if let Some((_, off)) = self.doors[*index].cell {
                                step(self.door_node(*index), off);
                            }
                        }
                    }
                }
            }
            Err(Stop::Connector(index)) => {
                let connector = &self.connectors[index];
                for (other, cost) in &self.shafts[index] {
                    step(self.connector_node(*other), *cost);
                }
                if let Some(cell) = connector.cell {
                    let centre = self.centre(cell);
                    step(
                        self.cell_node(connector.floor, cell),
                        (centre.0 - connector.at.0).hypot(centre.1 - connector.at.1) as f32,
                    );
                }
                for (door, other, cost) in &self.blind {
                    if *other == index {
                        step(self.door_node(*door), *cost);
                    }
                }
            }
            Err(Stop::Door(index)) => {
                let door = &self.doors[index];
                if let Some((cell, off)) = door.cell {
                    step(self.cell_node(door.floor, cell), off);
                }
                for (other, connector, cost) in &self.blind {
                    if *other == index {
                        step(self.connector_node(*connector), *cost);
                    }
                }
            }
        }
    }

    /// The free cells where `unit` meets walkable space, on its floor.
    fn threshold(&self, map: &Map, unit: usize) -> Vec<u32> {
        let record = &map.export.units[unit];
        let levels = &map.export.buildings[self.building].levels;
        let Some(floor) = self
            .floors
            .iter()
            .position(|floor| levels[floor.level].id == record.level_id)
        else {
            return Vec::new();
        };
        let grid = &self.floors[floor];
        if grid.cells.is_empty() {
            return Vec::new();
        }
        let mut near = vec![false; self.width * self.height];
        self.fill(&self.rings(&record.polygon), |row, first, last| {
            let rows = row.saturating_sub(ROOM_REACH)..(row + ROOM_REACH + 1).min(self.height);
            let columns = first.saturating_sub(ROOM_REACH)..(last + ROOM_REACH).min(self.width);
            for j in rows {
                near[j * self.width + columns.start..j * self.width + columns.end].fill(true);
            }
        });
        (0..near.len())
            .filter(|cell| near[*cell] && grid.cells[*cell] & (WALK | WALL) == WALK)
            .map(|cell| self.cell_node(floor, cell))
            .collect()
    }

    /// The way to `unit` from everywhere in the building it can be walked
    /// to from; `None` when the unit meets no walkable space.
    pub(super) fn reach(&self, map: &Map, unit: usize) -> Option<Reach> {
        let threshold = self.threshold(map, unit);
        if threshold.is_empty() {
            return None;
        }
        let mut cost: HashMap<u32, f32> = HashMap::new();
        let mut toward: HashMap<u32, u32> = HashMap::new();
        let mut queue = BinaryHeap::new();
        for node in threshold {
            cost.insert(node, 0.0);
            queue.push(Visit { cost: 0.0, node });
        }
        while let Some(Visit { cost: so_far, node }) = queue.pop() {
            if cost.get(&node).is_some_and(|best| so_far > *best) {
                continue;
            }
            self.steps(node, |next, metres| {
                let reached = so_far + metres;
                if cost.get(&next).is_none_or(|best| reached < *best) {
                    cost.insert(next, reached);
                    toward.insert(next, node);
                    queue.push(Visit {
                        cost: reached,
                        node: next,
                    });
                }
            });
        }
        Some(Reach { cost, toward, unit })
    }

    /// The building's entrances the unit is reached from, each with the
    /// cost of the way in.
    pub(super) fn entrances(&self, reach: &Reach) -> Vec<(usize, f32)> {
        self.doors
            .iter()
            .enumerate()
            .filter_map(|(door, entry)| {
                reach
                    .cost
                    .get(&self.door_node(door))
                    .map(|cost| (entry.entrance, *cost))
            })
            .collect()
    }

    /// Whether a straight line between two cells of a floor stays on free
    /// cells at least `clear` cells from a wall.
    fn sees(&self, floor: &Floor, from: usize, to: usize, clear: u8) -> bool {
        let (a, b) = (self.centre(from), self.centre(to));
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) / (CELL_M / 4.0)).ceil() as usize + 1;
        (0..=steps).all(|step| {
            let along = step as f64 / steps as f64;
            self.cell_at((a.0 + (b.0 - a.0) * along, a.1 + (b.1 - a.1) * along))
                .is_some_and(|cell| floor.clear[cell] >= clear)
        })
    }

    /// A run of cells pulled straight: the corners a taut string would
    /// keep, staying half a metre off the walls where the corridor allows.
    fn straighten(&self, floor: &Floor, run: &[usize]) -> Vec<usize> {
        let mut kept = Vec::new();
        let mut anchor = 0;
        while anchor < run.len() {
            kept.push(run[anchor]);
            if anchor + 1 == run.len() {
                break;
            }
            let farthest = |clear: u8| {
                (anchor + 1..run.len())
                    .take_while(|next| self.sees(floor, run[anchor], run[*next], clear))
                    .last()
                    .unwrap_or(anchor + 1)
            };
            let wide = farthest(2);
            anchor = if wide > anchor + 1 { wide } else { farthest(1) };
        }
        kept
    }

    /// The route from `entrance` to the reach's unit, floor by floor, and
    /// the floors climbed by stairs and the lifts taken on it.
    pub(super) fn legs(
        &self,
        map: &Map,
        reach: &Reach,
        entrance: usize,
    ) -> Option<(Vec<Leg>, u32, u32)> {
        let door = self
            .doors
            .iter()
            .position(|door| door.entrance == entrance)?;
        let mut node = self.door_node(door);
        reach.cost.get(&node)?;
        let back = |at: (f64, f64)| geometry::offset(self.center, at.0, at.1);
        let mut legs: Vec<Leg> = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        let mut stairs = 0u32;
        // A leg is blind until a cell of its floor is walked.
        let open = |legs: &mut Vec<Leg>, floor: usize| {
            if legs
                .last()
                .is_none_or(|leg| leg.level != self.floors[floor].level)
            {
                legs.push(Leg {
                    level: self.floors[floor].level,
                    points: Vec::new(),
                    blind: true,
                    leaves_by: None,
                });
            }
        };
        let flush = |legs: &mut Vec<Leg>, run: &mut Vec<usize>, floor: usize| {
            if run.is_empty() {
                return;
            }
            let kept = self.straighten(&self.floors[floor], run);
            if let Some(leg) = legs.last_mut() {
                leg.points
                    .extend(kept.into_iter().map(|cell| back(self.centre(cell))));
            }
            run.clear();
        };
        let mut floor_of_run = self.doors[door].floor;
        loop {
            match self.stop(node) {
                Ok((floor, cell)) => {
                    open(&mut legs, floor);
                    floor_of_run = floor;
                    run.push(cell);
                    if let Some(leg) = legs.last_mut() {
                        leg.blind = false;
                    }
                }
                Err(Stop::Door(index)) => {
                    let entry = &self.doors[index];
                    open(&mut legs, entry.floor);
                    if let Some(leg) = legs.last_mut() {
                        leg.points.push(back(entry.at));
                    }
                }
                Err(Stop::Connector(index)) => {
                    let connector = &self.connectors[index];
                    flush(&mut legs, &mut run, floor_of_run);
                    open(&mut legs, connector.floor);
                    if let Some(leg) = legs.last_mut() {
                        leg.points.push(back(connector.at));
                    }
                    // A shaft link next: this leg ends here.
                    let next = reach.toward.get(&node).copied();
                    if let Some(Err(Stop::Connector(other))) = next.map(|next| self.stop(next)) {
                        let floors = self.floors[self.connectors[other].floor]
                            .ordinal
                            .abs_diff(self.floors[connector.floor].ordinal);
                        if connector.shaft == Shaft::Stairs {
                            stairs += floors;
                        }
                        if let Some(leg) = legs.last_mut() {
                            leg.leaves_by = Some(connector.shaft);
                        }
                    }
                }
            }
            match reach.toward.get(&node) {
                Some(next) => node = *next,
                None => break,
            }
        }
        flush(&mut legs, &mut run, floor_of_run);
        // Into the room, at its middle.
        let unit = &map.export.units[reach.unit];
        if let Some(leg) = legs.last_mut() {
            leg.points.push(unit.center);
        }
        // A floor a shaft only passes leaves a leg of one point: join the
        // legs either side of it.
        let mut joined: Vec<Leg> = Vec::new();
        for leg in legs {
            if leg.points.len() < 2 {
                if let (Some(last), Some(by)) = (joined.last_mut(), leg.leaves_by) {
                    last.leaves_by = Some(by);
                }
                continue;
            }
            joined.push(leg);
        }
        if let Some(last) = joined.last_mut() {
            last.blind = false;
        }
        let lifts = joined
            .iter()
            .filter(|leg| leg.leaves_by == Some(Shaft::Lift))
            .count() as u32;
        Some((joined, stairs, lifts))
    }
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

/// The outer ring's area, square metres.
fn area(rings: &[Vec<(f64, f64)>]) -> f64 {
    rings.first().map_or(0.0, |ring| {
        let twice: f64 = (0..ring.len())
            .map(|index| {
                let (a, b) = (ring[index], ring[(index + 1) % ring.len()]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum();
        (twice / 2.0).abs()
    })
}

// MARK: Plans

/// The bundled walkways, read once.
pub(super) fn walkways() -> &'static Walkways {
    static WALKWAYS: OnceLock<Walkways> = OnceLock::new();
    WALKWAYS.get_or_init(Walkways::bundled)
}

/// A route as worked out: outdoors to an entrance, then the legs indoors.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Plan {
    /// From the person's position to the entrance; empty with no position.
    pub(super) outdoor: Vec<Position>,
    pub(super) legs: Vec<Leg>,
    /// Index in the building's entrances.
    pub(super) entrance: Option<usize>,
    /// Walked metres, outdoors and in.
    pub(super) metres: f64,
    pub(super) stair_floors: u32,
    pub(super) lifts: u32,
    /// False when a room was asked for and the way to it indoors is not
    /// known: the route ends at an entrance.
    pub(super) complete: bool,
}

/// The route to `unit` of `building` (else to the building, by `entrance`
/// when one is named), from `from` when the person's position is known and
/// on the walkways. `None` when there is nothing to draw: no position, and
/// no way known indoors.
pub(super) fn plan(
    map: &Map,
    walkways: &Walkways,
    from: Option<Position>,
    building: usize,
    unit: Option<usize>,
    entrance: Option<usize>,
) -> Option<Plan> {
    let record = &map.export.buildings[building];
    let indoor = unit.and_then(|unit| {
        let indoor = Indoor::new(map, building)?;
        let reach = indoor.reach(map, unit)?;
        Some((indoor, reach))
    });
    // The entrances to weigh, each with the cost of the way in from it.
    let mut ways: Vec<(usize, f32)> = indoor
        .as_ref()
        .map(|(indoor, reach)| indoor.entrances(reach))
        .unwrap_or_default();
    if let Some(entrance) = entrance {
        ways.retain(|(way, _)| *way == entrance);
    }
    let complete = unit.is_none() || !ways.is_empty();
    if ways.is_empty() {
        ways = match entrance {
            Some(entrance) => vec![(entrance, 0.0)],
            None => (0..record.entrances.len()).map(|way| (way, 0.0)).collect(),
        };
    }
    // Outdoors: from the person to each entrance.
    let outdoors = from.and_then(|from| {
        let mut walkways = walkways.clone();
        let start = walkways.join(from, ORIGIN_SNAP_M)?;
        let ends: Vec<Option<u32>> = ways
            .iter()
            .map(|(way, _)| walkways.join(record.entrances[*way].point, ENTRANCE_SNAP_M))
            .collect();
        let (cost, before) = walkways.spread(start);
        let (best, end) = ways
            .iter()
            .zip(&ends)
            .enumerate()
            .filter_map(|(index, ((_, inside), end))| {
                let end = (*end)?;
                cost[end as usize]
                    .is_finite()
                    .then(|| (index, end, cost[end as usize] + inside))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(index, end, _)| (index, end))?;
        let mut path = vec![walkways.nodes[end as usize]];
        let mut node = end;
        while before[node as usize] != u32::MAX {
            node = before[node as usize];
            path.push(walkways.nodes[node as usize]);
        }
        path.reverse();
        Some((best, path))
    });
    let (chosen, outdoor) = match outdoors {
        Some((best, path)) => (best, path),
        None => {
            // No position: only a way indoors is worth drawing.
            indoor.as_ref().filter(|_| complete && unit.is_some())?;
            let best = (0..ways.len()).min_by(|a, b| ways[*a].1.total_cmp(&ways[*b].1))?;
            (best, Vec::new())
        }
    };
    let entrance = ways[chosen].0;
    let (legs, stair_floors, lifts) = indoor
        .as_ref()
        .filter(|_| complete)
        .and_then(|(indoor, reach)| indoor.legs(map, reach, entrance))
        .unwrap_or_default();
    let length = |points: &[Position]| -> f64 {
        points
            .windows(2)
            .map(|pair| distance(pair[0], pair[1]))
            .sum()
    };
    let metres = length(&outdoor) + legs.iter().map(|leg| length(&leg.points)).sum::<f64>();
    Some(Plan {
        outdoor,
        legs,
        entrance: Some(entrance),
        metres,
        stair_floors,
        lifts,
        complete,
    })
}

#[cfg(test)]
#[path = "../tests/campus_map__route.rs"]
mod tests;
