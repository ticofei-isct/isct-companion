//! Routes over the published export, by fixed real places. As for the page
//! tests, `ISCT_CAMPUS_MAP_EXPORT` names the pinned `campus-map.json.gz`,
//! so these run only with `--ignored`.
use super::*;
use crate::campus_map::export;

/// 大岡山西9号館: entered on 1F and 2F, corridors drawn on 2F and 3F.
const W9: &str = "a10026fc-4e59-55a7-9d89-0193f1749620";
/// By the main building's front door.
const BY_THE_MAIN_BUILDING: Position = [139.683902, 35.604681];

fn map() -> Map {
    let path = std::env::var("ISCT_CAMPUS_MAP_EXPORT").unwrap_or_else(|_| {
        panic!("ISCT_CAMPUS_MAP_EXPORT must name the published campus-map.json.gz")
    });
    let gz = std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    Map::new(export::read(&gz).expect("format 1")).expect("a valid export")
}

fn unit(map: &Map, label: &str) -> usize {
    map.export
        .units
        .iter()
        .position(|unit| unit.label.as_deref() == Some(label))
        .unwrap_or_else(|| panic!("{label} is on the plans"))
}

#[test]
#[ignore = "needs the published export in ISCT_CAMPUS_MAP_EXPORT"]
fn a_route_follows_the_walkways_and_the_plans_to_a_room() {
    let map = map();
    let walkways = Walkways::bundled();
    let w9 = map.buildings[W9];
    let record = &map.export.buildings[w9];
    let room = unit(&map, "W9-324");
    let found = plan(
        &map,
        &walkways,
        Some(BY_THE_MAIN_BUILDING),
        w9,
        Some(room),
        None,
    )
    .expect("a route to W9-324");
    assert!(found.complete);
    // Outdoors from the person to the entrance chosen, on the walkways:
    // longer than the straight line, not absurdly so.
    let entrance = &record.entrances[found.entrance.expect("an entrance")];
    let first = *found.outdoor.first().expect("an outdoor part");
    let last = *found.outdoor.last().expect("an outdoor part");
    assert!(distance(first, BY_THE_MAIN_BUILDING) < 1.0);
    assert!(distance(last, entrance.point) < 1.0);
    let line = distance(BY_THE_MAIN_BUILDING, map.export.units[room].center);
    assert!(found.metres > line && found.metres < 3.0 * line);
    // Indoors from the entrance's floor to the room's, each floor left at
    // a stair or lift and the next entered at the same shaft.
    let level = |leg: &Leg| record.levels[leg.level].id.clone();
    let legs = &found.legs;
    assert_eq!(Some(level(&legs[0])), entrance.level_id);
    assert_eq!(
        level(&legs[legs.len() - 1]),
        map.export.units[room].level_id
    );
    assert!(distance(legs[0].points[0], entrance.point) < 1.0);
    assert_eq!(
        legs[legs.len() - 1].points.last(),
        Some(&map.export.units[room].center)
    );
    assert!(legs.len() >= 2);
    for pair in legs.windows(2) {
        assert!(pair[0].leaves_by.is_some());
        let (below, above) = (pair[0].points.last(), pair[1].points.first());
        assert!(distance(*below.expect("a leg"), *above.expect("a leg")) <= SHAFT_REACH_M);
    }
    assert!(legs[legs.len() - 1].leaves_by.is_none());
    assert!(found.stair_floors + found.lifts >= 1);
    // No leg cuts through a room: every stretch between two corners stays
    // on walkable cells, but for the last step into the room itself.
    let indoor = Indoor::new(&map, w9).expect("W9 has corridors");
    for leg in legs.iter().filter(|leg| !leg.blind) {
        let floor = indoor
            .floors
            .iter()
            .find(|floor| floor.level == leg.level)
            .expect("the leg's floor");
        let inner = &leg.points[1..leg.points.len() - 1];
        for pair in inner.windows(2) {
            let (a, b) = (indoor.local(pair[0]), indoor.local(pair[1]));
            for step in 0..=20 {
                let along = f64::from(step) / 20.0;
                let at = (a.0 + (b.0 - a.0) * along, a.1 + (b.1 - a.1) * along);
                let cell = indoor.cell_at(at).expect("inside the grid");
                assert!(floor.cells[cell] & WALK != 0, "a leg leaves walkable space");
            }
        }
    }
    // With no position the route is the indoor part alone; a room on a
    // floor with no corridors drawn ends at an entrance, and only when
    // there is a position to come from.
    let inside = plan(&map, &walkways, None, w9, Some(room), None).expect("the way in");
    assert!(inside.outdoor.is_empty() && inside.complete && !inside.legs.is_empty());
    let unmapped = map
        .export
        .units
        .iter()
        .position(|unit| {
            unit.label.is_some()
                && map.levels.get(&unit.level_id).is_some_and(|(building, _)| {
                    Indoor::new(&map, *building).is_none()
                        && !map.export.buildings[*building].entrances.is_empty()
                })
        })
        .expect("a named room in a building with no corridors drawn");
    let (building, _) = map.levels[&map.export.units[unmapped].level_id];
    let campus = map.export.buildings[building].center;
    let to_door = plan(
        &map,
        &walkways,
        Some(campus),
        building,
        Some(unmapped),
        None,
    )
    .expect("a route to the entrance");
    assert!(!to_door.complete && to_door.legs.is_empty() && to_door.outdoor.len() >= 2);
    assert!(plan(&map, &walkways, None, building, Some(unmapped), None).is_none());
}

#[test]
#[ignore = "needs the published export in ISCT_CAMPUS_MAP_EXPORT"]
fn the_named_rooms_on_floors_with_corridors_are_reached_from_an_entrance() {
    let map = map();
    // Every named room on a floor whose corridors are drawn: whether a way
    // in from an entrance is known. The plans leave some out (a floor whose
    // corridors touch no stair, a building with no entrance in the export);
    // this is the count on the pinned export, so a change that loses routes
    // shows.
    let (mut reached, mut total) = (0, 0);
    for building in 0..map.export.buildings.len() {
        let Some(indoor) = Indoor::new(&map, building) else {
            continue;
        };
        for floor in indoor.floors.iter().filter(|floor| !floor.cells.is_empty()) {
            let level = &map.export.buildings[building].levels[floor.level];
            for unit in map.units_by_level.get(&level.id).into_iter().flatten() {
                let record = &map.export.units[*unit];
                if walkable(record.category) || (record.label.is_none() && record.room_id.is_none())
                {
                    continue;
                }
                total += 1;
                let known = indoor
                    .reach(&map, *unit)
                    .is_some_and(|reach| !indoor.entrances(&reach).is_empty());
                reached += usize::from(known);
            }
        }
    }
    assert_eq!((reached, total), (93, 114));
}
