mod app;
mod debug;
mod error;
mod isochrone;
mod journey;
mod routing;
mod service;
mod utils;

#[cfg(feature = "hectare")]
pub use app::run_surface_per_ha;
pub use app::{run_average, run_comparison, run_optimal, run_simple, run_worst};
pub use debug::run_debug;
pub use error::RResult;
pub use isochrone::externals::{ExcludedPolygons, LAKES_GEOJSON_URLS};
pub use isochrone::{IsochroneArgs, IsochroneDisplayMode};
#[cfg(feature = "hectare")]
pub use isochrone::{IsochroneHectareArgs, externals::HectareData};
pub use journey::{JourneyArgs, ReverseJourneyArgs};
pub use routing::{Route, plan_journey, plan_journey_reverse, plan_shortest_journey};
pub use service::run_service;

#[cfg(test)]
mod tests {
    use std::{env, error::Error, fs::read_to_string, time::Instant};

    use crate::{
        ExcludedPolygons, HectareData, LAKES_GEOJSON_URLS,
        isochrone::unique_coordinates_from_routes,
        routing::{compute_routes_from_origin, plan_shortest_journey_with_reverse},
        utils::create_date_time,
    };
    use chrono::{Duration, TimeDelta, Timelike};
    use hrdf_parser::Hrdf;
    use ojp_rs::{OJP, SimplifiedLeg, SimplifiedTrip};

    use test_log::test;

    use crate::{Route, plan_journey, plan_journey_reverse, plan_shortest_journey};
    use futures::future::join_all;

    use pretty_assertions::assert_eq;

    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use serde::{Deserialize, Serialize};

    fn get_json_values<F>(
        lhs: &F,
        rhs: &str,
    ) -> Result<(serde_json::Value, serde_json::Value), Box<dyn Error>>
    where
        for<'a> F: Serialize + Deserialize<'a>,
    {
        let serialized = serde_json::to_string(&lhs)?;
        let reference = serde_json::to_string(&serde_json::from_str::<F>(rhs)?)?;
        Ok((
            serialized.parse::<serde_json::Value>()?,
            reference.parse::<serde_json::Value>()?,
        ))
    }

    struct STrip(SimplifiedTrip);

    impl STrip {
        fn from(value: &Route, hrdf: &Hrdf) -> Self {
            let mut prev_arr_time = value.departure_at();
            let legs = value
                .sections()
                .iter()
                .map(|s| {
                    let departure_id = s.departure_stop_id();
                    let departure_stop = s.departure_stop_name(hrdf.data_storage());
                    let arrival_id = s.arrival_stop_id();
                    let arrival_stop = s.arrival_stop_name(hrdf.data_storage());
                    let departure_time = s.departure_at().unwrap_or(prev_arr_time);
                    let arrival_time = s.arrival_at().unwrap_or(
                        prev_arr_time + TimeDelta::minutes(s.duration().unwrap_or(0) as i64),
                    );
                    prev_arr_time = arrival_time;
                    SimplifiedLeg::new(
                        departure_id,
                        departure_stop,
                        arrival_id,
                        arrival_stop,
                        departure_time,
                        arrival_time,
                        format!("{:?}", s.transport()),
                    )
                })
                .collect::<Vec<_>>();
            STrip(SimplifiedTrip::new(legs))
        }
    }

    static IDS: [(i32, i32); 34] = [
        (8577820, 8501120),
        (8572662, 8576724),
        (8593320, 8579237),
        (8592862, 8500236),
        (8592458, 8595922),
        (8591921, 8589143),
        (8591915, 8583275),
        (8591611, 8595689),
        (8590925, 8592776),
        (8589645, 8583274),
        (8589632, 8591245),
        (8589164, 8592567),
        (8591610, 8575154),
        (8591921, 8581062),
        (8592837, 8588351),
        (8591363, 8504100),
        (8580798, 8588731),
        (8592587, 8593462),
        (8596094, 8589007),
        (8583005, 8591046),
        (8589151, 8592547),
        (8588949, 8580456),
        (8573693, 8504354),
        (8509076, 8587619),
        (8501120, 8579006),
        (8591418, 8592834),
        (8570732, 8573673),
        (8578997, 8576815),
        (8585206, 8506302),
        (8589587, 8592133),
        (22, 8592904),
        (8592889, 8589566),
        (8572453, 8591998),
        (8500236, 8511236),
    ];

    pub async fn test_paths_validity_reverse(
        hrdf: &Hrdf,
        ids: &[(i32, i32)],
    ) -> Result<Vec<(Option<SimplifiedTrip>, Option<SimplifiedTrip>)>, Box<dyn Error>> {
        let ref_trips = ids
            .iter()
            .map(|(from_id, to_id)| {
                let fname = format!("test_xml/{from_id}_{to_id}_trip.xml");
                let xml = std::fs::read_to_string(fname).unwrap();
                let ojp = OJP::try_from(xml.as_str()).unwrap();

                let ref_trip = ojp.fastest_trip().unwrap();

                SimplifiedTrip::try_from(ref_trip).unwrap()
            })
            .collect::<Vec<_>>();

        let hrdf_trips = ref_trips
            .iter()
            .map(|st| async move {
                let from_id = st.departure_id();
                let to_id = st.arrival_id();
                let date_time = st.departure_time().with_second(0).unwrap();
                log::info!("Testing trip: {from_id} - {to_id} at {date_time}");
                plan_shortest_journey_with_reverse(hrdf, from_id, to_id, date_time, 10, false)
                    .as_ref()
                    .map(|r| STrip::from(r, hrdf).0)
            })
            .collect::<Vec<_>>();
        let hrdf_trips: Vec<_> = join_all(hrdf_trips).await;
        // We are only interested in the "failures" of the hrdf routing engine
        let failed_comparison = ref_trips
            .into_iter()
            .zip(hrdf_trips.into_iter())
            .filter_map(|(rt, ht)| {
                if let Some(ht) = ht
                    && !rt.approx_equal(&ht, 0.1)
                {
                    Some((Some(rt), Some(ht)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok(failed_comparison)
    }

    pub async fn test_paths_validity_consistency(
        hrdf: &Hrdf,
        ids: &[(i32, i32)],
    ) -> Result<Vec<(Option<SimplifiedTrip>, Option<SimplifiedTrip>)>, Box<dyn Error>> {
        let ref_trips = ids
            .iter()
            .map(|(from_id, to_id)| {
                let fname = format!("test_xml/{from_id}_{to_id}_trip.xml");
                let xml = std::fs::read_to_string(fname).unwrap();
                let ojp = OJP::try_from(xml.as_str()).unwrap();

                let ref_trip = ojp.fastest_trip().unwrap();

                SimplifiedTrip::try_from(ref_trip).unwrap()
            })
            .collect::<Vec<_>>();

        let hrdf_trips = ref_trips
            .iter()
            .map(|st| async move {
                let from_id = st.departure_id();
                let to_id = st.arrival_id();
                let date_time = st.departure_time().with_second(0).unwrap();
                log::info!("Testing trip forward: {from_id} - {to_id} at {date_time}");
                plan_shortest_journey(hrdf, from_id, to_id, date_time, 11, false)
                    .as_ref()
                    .map(|r| STrip::from(r, hrdf).0)
            })
            .collect::<Vec<_>>();
        let hrdf_trips: Vec<_> = join_all(hrdf_trips).await;

        let hrdf_trips_reverse = ref_trips
            .iter()
            .map(|st| async move {
                let from_id = st.departure_id();
                let to_id = st.arrival_id();
                let date_time = st.departure_time().with_second(0).unwrap();
                log::info!("Testing trip reverse: {from_id} - {to_id} at {date_time}");
                plan_shortest_journey_with_reverse(hrdf, from_id, to_id, date_time, 11, false)
                    .as_ref()
                    .map(|r| STrip::from(r, hrdf).0)
            })
            .collect::<Vec<_>>();
        let hrdf_trips_reverse: Vec<_> = join_all(hrdf_trips_reverse).await;

        // We are only interested in the "failures" of the hrdf routing engine
        let failed_comparison = hrdf_trips
            .into_iter()
            .zip(hrdf_trips_reverse.into_iter())
            .filter_map(|(rt, ht)| match (rt, ht) {
                (Some(rt), Some(ht)) => {
                    if !rt.approx_equal(&ht, 0.1) {
                        Some((Some(rt), Some(ht)))
                    } else {
                        None
                    }
                }
                (Some(rt), None) => Some((Some(rt), None)),
                (None, Some(rt)) => Some((None, Some(rt))),
                _ => None,
            })
            .collect::<Vec<_>>();

        Ok(failed_comparison)
    }

    pub async fn test_paths_validity(
        hrdf: &Hrdf,
        ids: &[(i32, i32)],
    ) -> Result<Vec<(Option<SimplifiedTrip>, Option<SimplifiedTrip>)>, Box<dyn Error>> {
        let ref_trips = ids
            .iter()
            .map(|(from_id, to_id)| {
                let fname = format!("test_xml/{from_id}_{to_id}_trip.xml");
                let xml = std::fs::read_to_string(fname).unwrap();
                let ojp = OJP::try_from(xml.as_str()).unwrap();

                let ref_trip = ojp.fastest_trip().unwrap();

                SimplifiedTrip::try_from(ref_trip).unwrap()
            })
            .collect::<Vec<_>>();

        let hrdf_trips = ref_trips
            .iter()
            .map(|st| async move {
                let from_id = st.departure_id();
                let to_id = st.arrival_id();
                let date_time = st.departure_time().with_second(0).unwrap();
                log::info!("Testing trip: {from_id} - {to_id} at {date_time}");
                plan_shortest_journey(hrdf, from_id, to_id, date_time, 10, false)
                    .as_ref()
                    .map(|r| STrip::from(r, hrdf).0)
            })
            .collect::<Vec<_>>();
        let hrdf_trips: Vec<_> = join_all(hrdf_trips).await;
        // We are only interested in the "failures" of the hrdf routing engine
        let failed_comparison = ref_trips
            .into_iter()
            .zip(hrdf_trips.into_iter())
            .filter_map(|(rt, ht)| {
                if let Some(ht) = ht
                    && !rt.approx_equal(&ht, 0.1)
                {
                    Some((Some(rt), Some(ht)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok(failed_comparison)
    }

    pub fn test_find_reachable_stops_within_time_limit(hrdf: &Hrdf) {
        let max_num_explorable_connections = 10;
        let mut departures = Vec::new();
        // 1. Petit-Lancy, Les Esserts (8587418)
        let departure_stop_id = 8587418;
        let departure_at = create_date_time(2025, 6, 1, 12, 30);
        departures.push((departure_stop_id, departure_at));

        // 2. Sevelen, Post (8588197)
        let departure_stop_id = 8588197;
        let departure_at = create_date_time(2025, 9, 2, 14, 2);
        departures.push((departure_stop_id, departure_at));

        // 3. Avully, village (8587031)
        let departure_stop_id = 8587031;
        let departure_at = create_date_time(2025, 7, 13, 16, 43);
        departures.push((departure_stop_id, departure_at));

        // 4. Bern, Bierhübeli (8590028)
        let departure_stop_id = 8590028;
        let departure_at = create_date_time(2025, 9, 17, 5, 59);
        departures.push((departure_stop_id, departure_at));

        // 5. Genève, gare Cornavin (8587057)
        let departure_stop_id = 8587057;
        let departure_at = create_date_time(2025, 10, 18, 20, 10);
        departures.push((departure_stop_id, departure_at));

        // 6. Villmergen, Zentrum (8587554)
        let departure_stop_id = 8587554;
        let departure_at = create_date_time(2025, 11, 22, 6, 59);
        departures.push((departure_stop_id, departure_at));

        // 7. Lugano, Genzana (8575310)
        let departure_stop_id = 8575310;
        let departure_at = create_date_time(2025, 4, 9, 8, 4);
        departures.push((departure_stop_id, departure_at));

        // 8. Zürich HB (8503000)
        let departure_stop_id = 8503000;
        let departure_at = create_date_time(2025, 6, 15, 12, 10);
        departures.push((departure_stop_id, departure_at));

        // 9. Campocologno (8509368)
        let departure_stop_id = 8509368;
        let departure_at = create_date_time(2025, 5, 29, 17, 29);
        departures.push((departure_stop_id, departure_at));

        // 10. Chancy, Douane (8587477)
        let departure_stop_id = 8587477;
        let departure_at = create_date_time(2025, 9, 10, 13, 37);
        departures.push((departure_stop_id, departure_at));

        let start_time = Instant::now();
        let time_limit = 60;
        for (departure_stop_id, departure_at) in departures.into_iter() {
            let coordinates = hrdf
                .data_storage()
                .stops()
                .data()
                .get(&departure_stop_id)
                .unwrap()
                .wgs84_coordinates()
                .unwrap();
            let routes = compute_routes_from_origin(
                hrdf,
                coordinates.latitude().unwrap(),
                coordinates.longitude().unwrap(),
                departure_at,
                Duration::minutes(time_limit),
                1,
                1,
                max_num_explorable_connections,
                false,
            );
            let mut data = unique_coordinates_from_routes(&routes, departure_at)
                .into_iter()
                .map(|(c, td)| {
                    (
                        c.easting().unwrap(),
                        c.northing().unwrap(),
                        td.num_minutes(),
                    )
                })
                .collect::<Vec<_>>();
            data.sort_by(|(la, lb, lc), (ra, rb, rc)| {
                let first = lc.cmp(rc);
                match first {
                    std::cmp::Ordering::Equal => {
                        let second = la.partial_cmp(ra).unwrap();
                        match second {
                            std::cmp::Ordering::Equal => lb.partial_cmp(rb).unwrap(),
                            _ => second,
                        }
                    }
                    _ => first,
                }
            });
            let fname = format!("test_json/ref_routes_{departure_stop_id}.json");
            eprintln!("Comparing {fname}");
            let reference = read_to_string(fname).unwrap();
            let (current, reference) = get_json_values(&data, &reference).unwrap();
            assert_eq!(current, reference);
        }

        println!("{:.2?}", start_time.elapsed());
    }

    #[ignore]
    #[test(tokio::test)]
    async fn test_journeys() {
        // First build hrdf file
        let hrdf = Hrdf::try_from_year(2025, false, None).await.unwrap();
        let started = Instant::now();
        let failures = test_paths_validity(&hrdf, &IDS).await.unwrap();
        log::info!(
            "Time elapsed for all the HRDF tests: {:?}",
            started.elapsed()
        );
        for f in failures.iter() {
            if let (Some(ojp_trip), Some(hrdf_trip)) = f {
                eprintln!("{} - {}", ojp_trip.departure_id(), ojp_trip.arrival_id());
                eprintln!("OJP: \n{ojp_trip}");
                eprintln!("HRDF: \n{hrdf_trip}");
            }
        }
        assert!(failures.is_empty());
    }

    #[test(tokio::test)]
    async fn test_reachables_stops() {
        // First build hrdf file
        let hrdf = Hrdf::try_from_year(2025, false, None).await.unwrap();
        let started = Instant::now();
        test_find_reachable_stops_within_time_limit(&hrdf);
        log::info!(
            "Time elapsed for all the HRDF tests: {:?}",
            started.elapsed()
        );
    }

    #[ignore]
    #[test(tokio::test)]
    async fn test_journeys_consistency() {
        // First build hrdf file
        let hrdf = Hrdf::try_from_year(2025, false, None).await.unwrap();
        let started = Instant::now();
        let failures = test_paths_validity_consistency(&hrdf, &IDS).await.unwrap();
        log::info!(
            "Time elapsed for all the HRDF tests: {:?}",
            started.elapsed()
        );
        for f in failures.iter() {
            match f {
                (Some(forward_trip), Some(backward_trip)) => {
                    eprintln!(
                        "{} - {}",
                        forward_trip.departure_id(),
                        forward_trip.arrival_id()
                    );
                    eprintln!("FORWARD: \n{forward_trip}");
                    eprintln!("REVERSE: \n{backward_trip}");
                }
                (Some(forward_trip), None) => {
                    eprintln!(
                        "{} - {}",
                        forward_trip.departure_id(),
                        forward_trip.arrival_id()
                    );
                    eprintln!("FORWARD: \n{forward_trip}");
                    eprintln!("REVERSE: \nNONE FOUND");
                }
                (None, Some(backward_trip)) => {
                    eprintln!(
                        "{} - {}",
                        backward_trip.departure_id(),
                        backward_trip.arrival_id()
                    );
                    eprintln!("FORWARD: \nNONE FOUND");
                    eprintln!("REVERSE: \n{backward_trip}");
                }
                _ => {}
            }
        }
        assert!(failures.is_empty());
    }

    #[test(tokio::test)]
    async fn test_real_polygons_cache() {
        let original = ExcludedPolygons::try_new(
            &LAKES_GEOJSON_URLS,
            true,
            Some(env::temp_dir().to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to create new polygons from online data");
        let loaded = ExcludedPolygons::try_new(
            &LAKES_GEOJSON_URLS,
            false,
            Some(env::temp_dir().to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to create new polygons from cached");

        assert_eq!(original, loaded);
    }

    #[test(tokio::test)]
    #[cfg(feature = "hectare")]
    async fn test_real_hectare_data_cache() {
        use std::env;

        let url = "https://dam-api.bfs.admin.ch/hub/api/dam/assets/32686751/master";
        let original = HectareData::new(
            url,
            true,
            Some(env::temp_dir().to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to create new hectare data from online data");
        let loaded = HectareData::new(
            url,
            false,
            Some(env::temp_dir().to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to create new polygons from cached");

        assert_eq!(original, loaded);
    }

    #[test(tokio::test)]
    async fn test_reverse_journey_consistency() {
        let hrdf = Hrdf::try_from_year(2025, false, None).await.unwrap();

        // // Case 1: Simple direct trip
        // // Zürich HB (8503000) -> Bern (8507000)
        // let dep_stop = 8503000;
        // let arr_stop = 8507000;
        // let dep_time = create_date_time(2025, 6, 15, 10, 0); // 10:00
        //
        // println!("Testing Forward: Zürich -> Bern @ 10:00");
        // let forward_route = plan_journey(&hrdf, dep_stop, arr_stop, dep_time, 5, true).unwrap();
        // let arrival_time = forward_route.arrival_at();
        // println!(
        //     "Forward Found: Dep {:?} -> Arr {:?}",
        //     forward_route.departure_at(),
        //     arrival_time
        // );
        //
        // println!(
        //     "Testing Reverse: Zürich -> Bern arriving by {:?}",
        //     arrival_time
        // );
        // let reverse_route =
        //     plan_journey_reverse(&hrdf, dep_stop, arr_stop, arrival_time, 5, true).unwrap();
        // println!(
        //     "Reverse Found: Dep {:?} -> Arr {:?}",
        //     reverse_route.departure_at(),
        //     reverse_route.arrival_at()
        // );
        //
        // assert!(
        //     reverse_route.departure_at() >= forward_route.departure_at(),
        //     "Reverse departure {:?} should be >= Forward departure {:?}",
        //     reverse_route.departure_at(),
        //     forward_route.departure_at()
        // );
        // assert!(
        //     reverse_route.arrival_at() == arrival_time,
        //     "Reverse arrival {:?} should be == Requested arrival {:?}",
        //     reverse_route.arrival_at(),
        //     arrival_time
        // );
        //
        // // Case 2: Trip with Transfer
        // // Zürich HB (8503000) -> Zermatt (8501689)
        // let arr_stop_zermatt = 8501689;
        // println!("Testing Forward: Zürich -> Zermatt @ 08:00");
        // let dep_time_zermatt = create_date_time(2025, 6, 15, 8, 0);
        // let forward_route_z =
        //     plan_journey(&hrdf, dep_stop, arr_stop_zermatt, dep_time_zermatt, 8, true).unwrap();
        // let arrival_time_z = forward_route_z.arrival_at();
        // println!(
        //     "Forward Found: Dep {:?} -> Arr {:?}",
        //     forward_route_z.departure_at(),
        //     arrival_time_z
        // );
        //
        // println!(
        //     "Testing Reverse: Zürich -> Zermatt arriving by {:?}",
        //     arrival_time_z
        // );
        // let reverse_route_z =
        //     plan_journey_reverse(&hrdf, dep_stop, arr_stop_zermatt, arrival_time_z, 8, true)
        //         .unwrap();
        // println!(
        //     "Reverse Found: Dep {:?} -> Arr {:?}",
        //     reverse_route_z.departure_at(),
        //     reverse_route_z.arrival_at()
        // );
        //
        // assert!(
        //     reverse_route_z.departure_at() >= forward_route_z.departure_at(),
        //     "Reverse departure {:?} should be >= Forward departure {:?}",
        //     reverse_route_z.departure_at(),
        //     forward_route_z.departure_at()
        // );
        // assert!(
        //     reverse_route_z.arrival_at() == forward_route_z.arrival_at(),
        //     "Reverse arrival {:?} should be == Requested arrival {:?}",
        //     reverse_route_z.arrival_at(),
        //     forward_route_z.arrival_at()
        // );

        // Case 3: Trip with Transfer
        // Lausanne (8501120) -> Lugano, Vignola (8579006)
        let arr_stop_lausanne = 8501120;
        let arr_stop_lugano = 8579006;
        println!("Testing Forward: Lausanne -> Lugano, Vignola @ 05:40");
        let dep_time_lausanne = create_date_time(2025, 11, 25, 5, 40);
        let forward_route_l = plan_journey(
            &hrdf,
            arr_stop_lausanne,
            arr_stop_lugano,
            dep_time_lausanne,
            8,
            true,
        )
        .unwrap();
        let arrival_time_l = forward_route_l.arrival_at();
        println!(
            "Forward Found: Dep {:?} -> Arr {:?}",
            forward_route_l.departure_at(),
            arrival_time_l
        );

        println!(
            "Testing Reverse: Lausanne -> Lugano, Vignola arriving by {:?}",
            arrival_time_l
        );
        let reverse_route_l = plan_journey_reverse(
            &hrdf,
            arr_stop_lausanne,
            arr_stop_lugano,
            arrival_time_l,
            8,
            true,
        )
        .unwrap();
        println!(
            "Reverse Found: Dep {:?} -> Arr {:?}",
            reverse_route_l.departure_at(),
            reverse_route_l.arrival_at()
        );

        assert!(
            reverse_route_l.departure_at() >= forward_route_l.departure_at(),
            "Reverse departure {:?} should be >= Forward departure {:?}",
            reverse_route_l.departure_at(),
            forward_route_l.departure_at()
        );
        assert!(
            reverse_route_l.arrival_at() == forward_route_l.arrival_at(),
            "Reverse arrival {:?} should be == Requested arrival {:?}",
            reverse_route_l.arrival_at(),
            forward_route_l.arrival_at()
        );
    }
}
