//! Routes over the walk network inferred from the published export. Like
//! the page's rules they need the file (`ISCT_CAMPUS_MAP_EXPORT`), so they
//! run only with `--ignored`, in the `campus-map` workflow.
use super::*;
use crate::campus_map::export::{self, UnitCategory};
use std::sync::Arc;

/// 大岡山西9号館.
const W9: &str = "a10026fc-4e59-55a7-9d89-0193f1749620";

fn map() -> Arc<Map> {
    let path = std::env::var("ISCT_CAMPUS_MAP_EXPORT").unwrap_or_else(|_| {
        panic!("ISCT_CAMPUS_MAP_EXPORT must name the published campus-map.json.gz")
    });
    let gz = std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    Arc::new(Map::new(export::read(&gz).expect("format 1")).expect("a valid export"))
}

/// The unit a room code is drawn as.
fn unit(map: &Map, code: &str) -> usize {
    map.export
        .units
        .iter()
        .position(|unit| unit.label.as_deref() == Some(code))
        .unwrap_or_else(|| panic!("{code} is on the plans"))
}

#[test]
#[ignore = "needs the published export in ISCT_CAMPUS_MAP_EXPORT"]
fn most_classrooms_on_the_plans_can_be_reached_from_an_entrance() {
    // The plans show no doors, so some rooms stay out of reach; the share
    // that can be reached is what the inference is worth, and a change to
    // it that drops this far is a broken rule, not thinner data.
    let map = map();
    let (mut classrooms, mut reached) = (0, 0);
    for (building, record) in map.export.buildings.iter().enumerate() {
        let walk = map.walk(building);
        for level in &record.levels {
            for unit in map.units_by_level.get(&level.id).into_iter().flatten() {
                if map.export.units[*unit].category != UnitCategory::Classroom {
                    continue;
                }
                classrooms += 1;
                if walk.route(None, &[*unit], false).is_some() {
                    reached += 1;
                }
            }
        }
    }
    assert!(classrooms > 100, "{classrooms} classrooms on the plans");
    assert!(
        f64::from(reached) >= 0.75 * f64::from(classrooms),
        "{reached} of {classrooms} classrooms reached"
    );
}

#[test]
#[ignore = "needs the published export in ISCT_CAMPUS_MAP_EXPORT"]
fn a_route_starts_at_an_entrance_and_ends_in_the_room_on_its_floor() {
    let map = map();
    let w9 = map.buildings[W9];
    let room = unit(&map, "W9-324");
    let room_level = map.levels[&map.export.units[room].level_id].1;
    let walk = map.walk(w9);
    let route = walk
        .route(None, &[room], false)
        .expect("W9-324 is reachable");
    let record = &map.export.buildings[w9];
    let entrance = &record.entrances[route.entrance];
    let first = route.legs.first().expect("a first leg");
    assert_eq!(first.points.first(), Some(&entrance.point));
    assert_eq!(
        Some(record.levels[first.level].id.as_str()),
        entrance.level_id.as_deref()
    );
    let last = route.legs.last().expect("a last leg");
    assert_eq!(last.level, room_level);
    assert_eq!(last.points.last(), Some(&map.export.units[room].center));
    // Each change of floor joins one leg to the next.
    assert_eq!(route.climbs.len(), route.legs.len() - 1);
    for (climb, pair) in route.climbs.iter().zip(route.legs.windows(2)) {
        assert_eq!((climb.from, climb.to), (pair[0].level, pair[1].level));
    }
    // Every point lies on the building's own plan.
    for point in route.legs.iter().flat_map(|leg| leg.points.iter()) {
        assert!(
            geometry::distance_to_edges(&record.footprint, *point) < 15.0
                || geometry::multipolygon_contains(&record.footprint, *point)
        );
    }

    let step_free = walk.route(None, &[room], true).expect("W9 has lifts");
    assert!(step_free.climbs.iter().all(|climb| climb.via == Via::Lift));
    assert!(step_free.length_m >= route.length_m);
}

#[test]
#[ignore = "needs the published export in ISCT_CAMPUS_MAP_EXPORT"]
fn a_route_from_a_chosen_entrance_starts_there() {
    let map = map();
    let w9 = map.buildings[W9];
    let room = unit(&map, "W9-324");
    let walk = map.walk(w9);
    let best = walk.route(None, &[room], false).expect("reachable");
    for entrance in 0..map.export.buildings[w9].entrances.len() {
        if let Some(route) = walk.route(Some(entrance), &[room], false) {
            assert_eq!(route.entrance, entrance);
            assert!(route.length_m >= best.length_m);
        }
    }
}
