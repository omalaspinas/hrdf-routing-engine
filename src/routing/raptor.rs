//! RAPTOR (Round-based Public Transit Optimized Router) algorithm implementation.
//!
//! This implementation uses the existing HRDF DataStorage structures directly,
//! without requiring a separate index.

use chrono::{NaiveDate, NaiveDateTime, Timelike};
use hrdf_parser::{DataStorage, Journey, Model};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::utils::add_minutes_to_date_time;

use super::{
    connections::{get_exchange_time, get_operating_journeys},
    models::{Route, RouteResult, RouteSection, RoutingAlgorithmArgs, RoutingAlgorithmMode},
    utils::get_stop_connections,
};

/// Label storing arrival information at a stop for a given round.
#[derive(Debug, Clone)]
struct RaptorLabel {
    /// The time we arrive at this stop
    arrival_time: NaiveDateTime,
    /// The journey we took to get here (None if walking transfer)
    journey_id: Option<i32>,
    /// The stop we came from
    from_stop_id: i32,
    /// Where we boarded the journey (same as from_stop_id for transfers)
    boarding_stop_id: i32,
    /// Walking duration in minutes (only set for transfers)
    transfer_duration: Option<i16>,
    /// The round in which the from_stop was reached (for reconstruction)
    source_round: usize,
}

/// Main RAPTOR computation function.
/// Replaces the exploration-based routing with a round-based approach.
pub fn raptor_compute(
    data_storage: &DataStorage,
    departure_stop_id: i32,
    departure_at: NaiveDateTime,
    max_rounds: i32,
    _verbose: bool,
    args: RoutingAlgorithmArgs,
) -> FxHashMap<i32, RouteResult> {
    let mut state = RaptorState::new(departure_stop_id, departure_at);

    // Process initial walking transfers from departure stop
    process_initial_transfers(data_storage, &mut state, departure_stop_id, departure_at);

    // Main RAPTOR rounds
    // max_rounds represents the number of allowed transfers, so we need max_rounds + 1 transit legs
    // Round 1 = direct trip (0 transfers), Round 2 = 1 transfer, etc.
    let max_transit_legs = max_rounds + 1;
    for round in 1..=max_transit_legs {
        if state.marked_stops.is_empty() {
            break;
        }

        // Collect all journeys serving marked stops
        let stops_to_process: Vec<i32> = state.marked_stops.drain().collect();

        // Process each marked stop - find journeys and traverse them
        for &stop_id in &stops_to_process {
            process_stop_journeys(data_storage, &mut state, stop_id, round as usize, &args);
        }

        // Process walking transfers from newly improved stops
        let transit_improved: Vec<i32> = state
            .round_improved_stops
            .drain()
            .filter(|&stop_id| {
                // Only process transfers from stops reached by transit (not by walking)
                state
                    .labels_per_round
                    .get(round as usize)
                    .and_then(|m| m.get(&stop_id))
                    .is_some_and(|label| label.journey_id.is_some())
            })
            .collect();

        for stop_id in transit_improved {
            process_transfers(data_storage, &mut state, stop_id, round as usize, &args);
        }

        // Early termination for one-to-one routing
        if args.mode() == RoutingAlgorithmMode::SolveFromDepartureStopToArrivalStop {
            let target = args.arrival_stop_id();
            if state.best_arrival.contains_key(&target) && state.marked_stops.is_empty() {
                break;
            }
        }
    }

    // Reconstruct and return results
    reconstruct_results(data_storage, &state, departure_stop_id, &args)
}

/// RAPTOR algorithm state
struct RaptorState {
    /// Labels per round: labels_per_round[round][stop_id] = label
    labels_per_round: Vec<FxHashMap<i32, RaptorLabel>>,
    /// Best arrival time at each stop across all rounds
    best_arrival: FxHashMap<i32, NaiveDateTime>,
    /// Stops that were improved and need to be processed in the next round
    marked_stops: FxHashSet<i32>,
    /// Stops improved in the current round (for transfer processing)
    round_improved_stops: FxHashSet<i32>,
}

impl RaptorState {
    fn new(departure_stop_id: i32, departure_at: NaiveDateTime) -> Self {
        let mut labels_per_round = Vec::new();
        let mut round_0 = FxHashMap::default();

        // Initialize round 0 with the departure stop
        round_0.insert(
            departure_stop_id,
            RaptorLabel {
                arrival_time: departure_at,
                journey_id: None,
                from_stop_id: departure_stop_id,
                boarding_stop_id: departure_stop_id,
                transfer_duration: None,
                source_round: 0,
            },
        );

        labels_per_round.push(round_0);

        let mut best_arrival = FxHashMap::default();
        best_arrival.insert(departure_stop_id, departure_at);

        let mut marked_stops = FxHashSet::default();
        marked_stops.insert(departure_stop_id);

        RaptorState {
            labels_per_round,
            best_arrival,
            marked_stops,
            round_improved_stops: FxHashSet::default(),
        }
    }

    /// Ensure we have storage for this round
    fn ensure_round(&mut self, round: usize) {
        while self.labels_per_round.len() <= round {
            self.labels_per_round.push(FxHashMap::default());
        }
    }

    /// Try to improve arrival at a stop. Returns true if improved.
    /// If `mark_for_exploration` is false, the stop won't be added to marked_stops
    /// (used for intermediate stops in one-to-many mode).
    fn try_improve(
        &mut self,
        stop_id: i32,
        arrival_time: NaiveDateTime,
        journey_id: Option<i32>,
        from_stop_id: i32,
        boarding_stop_id: i32,
        transfer_duration: Option<i16>,
        round: usize,
        source_round: usize,
        source_arrival_time: NaiveDateTime,
        time_limit: Option<NaiveDateTime>,
        mark_for_exploration: bool,
    ) -> bool {
        // Check time limit for one-to-many mode
        if let Some(limit) = time_limit {
            if arrival_time > limit {
                return false;
            }
        }

        // Validate temporal consistency: arrival must be after source arrival
        if arrival_time <= source_arrival_time {
            return false;
        }

        // Check if this improves the best known arrival
        let dominated = self
            .best_arrival
            .get(&stop_id)
            .is_some_and(|&best| arrival_time >= best);

        if dominated {
            return false;
        }

        // Update best arrival
        self.best_arrival.insert(stop_id, arrival_time);

        // Store label for this round
        self.ensure_round(round);
        self.labels_per_round[round].insert(
            stop_id,
            RaptorLabel {
                arrival_time,
                journey_id,
                from_stop_id,
                boarding_stop_id,
                transfer_duration,
                source_round,
            },
        );

        // Mark for next round (only for exchange points used in exploration)
        if mark_for_exploration {
            self.marked_stops.insert(stop_id);
            self.round_improved_stops.insert(stop_id);
        }

        true
    }

    /// Get the label for a stop from the previous round (or any earlier round)
    /// Returns the label and the round it was found in
    fn get_previous_label(
        &self,
        stop_id: i32,
        current_round: usize,
    ) -> Option<(&RaptorLabel, usize)> {
        // Check from the most recent round backwards
        for r in (0..current_round).rev() {
            if let Some(label) = self.labels_per_round.get(r).and_then(|m| m.get(&stop_id)) {
                return Some((label, r));
            }
        }
        None
    }
}

/// Process initial walking transfers from the departure stop
fn process_initial_transfers(
    data_storage: &DataStorage,
    state: &mut RaptorState,
    departure_stop_id: i32,
    departure_at: NaiveDateTime,
) {
    if let Some(connections) = get_stop_connections(data_storage, departure_stop_id) {
        for conn in connections {
            let to_stop_id = conn.stop_id_2();
            let arrival_time = add_minutes_to_date_time(departure_at, conn.duration().into());

            state.try_improve(
                to_stop_id,
                arrival_time,
                None,
                departure_stop_id,
                departure_stop_id,
                Some(conn.duration()),
                0,
                0,
                departure_at,
                None,
            );
        }
    }
}

/// Process all journeys departing from a stop
fn process_stop_journeys(
    data_storage: &DataStorage,
    state: &mut RaptorState,
    stop_id: i32,
    round: usize,
    args: &RoutingAlgorithmArgs,
) {
    // Get the label for how we reached this stop
    let Some((arrival_label, source_round)) = state.get_previous_label(stop_id, round) else {
        return;
    };
    let arrival_time = arrival_label.arrival_time;
    let previous_journey_id = arrival_label.journey_id;

    // Use arrival date for journey lookup (important for multi-day trips)
    let date = arrival_time.date();

    let time_limit = match args.mode() {
        RoutingAlgorithmMode::SolveFromDepartureStopToReachableArrivalStops => {
            Some(args.time_limit())
        }
        _ => None,
    };

    // For one-to-one mode, we need to track the target stop even if it's not an exchange point
    let target_stop_id = match args.mode() {
        RoutingAlgorithmMode::SolveFromDepartureStopToArrivalStop => Some(args.arrival_stop_id()),
        _ => None,
    };

    // Track which route hashes we've already processed to avoid duplicates
    let mut processed_routes: FxHashSet<u64> = FxHashSet::default();

    // Process journeys for current date (based on arrival time)
    let journeys = get_operating_journeys(data_storage, date, stop_id);
    process_journeys_for_date(
        data_storage,
        state,
        &journeys,
        stop_id,
        arrival_time,
        previous_journey_id,
        date,
        round,
        source_round,
        time_limit,
        target_stop_id,
        &mut processed_routes,
    );

    // Also check next day if we're in the late evening (after 20:00)
    if arrival_time.time().hour() >= 20 {
        let next_date = date.succ_opt().unwrap_or(date);
        let journeys_next_day = get_operating_journeys(data_storage, next_date, stop_id);
        process_journeys_for_date(
            data_storage,
            state,
            &journeys_next_day,
            stop_id,
            arrival_time,
            previous_journey_id,
            next_date,
            round,
            source_round,
            time_limit,
            target_stop_id,
            &mut processed_routes,
        );
    }
}

/// Process journeys for a specific date
fn process_journeys_for_date(
    data_storage: &DataStorage,
    state: &mut RaptorState,
    journeys: &[&Journey],
    stop_id: i32,
    arrival_time: NaiveDateTime,
    previous_journey_id: Option<i32>,
    date: NaiveDate,
    round: usize,
    source_round: usize,
    time_limit: Option<NaiveDateTime>,
    target_stop_id: Option<i32>,
    processed_routes: &mut FxHashSet<u64>,
) {
    // Sort journeys by departure time to process earliest first
    let mut sorted_journeys: Vec<_> = journeys
        .iter()
        .filter_map(|&j| {
            // Skip if this is the last stop
            if j.is_last_stop(stop_id, true).unwrap_or(true) {
                return None;
            }
            // Get departure time
            let dep_time = j.departure_at_of(stop_id, date).ok()?;
            // Skip if departure is before our arrival at this stop
            if dep_time < arrival_time {
                return None;
            }
            Some((j, dep_time))
        })
        .collect();

    sorted_journeys.sort_by_key(|(_, dep_time)| *dep_time);

    for (journey, _) in sorted_journeys {
        // Check if we've already processed a journey with the same route pattern
        if let Some(hash) = journey.hash_route(stop_id) {
            if processed_routes.contains(&hash) {
                continue;
            }
            processed_routes.insert(hash);
        }

        process_journey(
            data_storage,
            state,
            journey,
            stop_id,
            arrival_time,
            previous_journey_id,
            date,
            round,
            source_round,
            time_limit,
            target_stop_id,
        );
    }
}

/// Process a single journey: check if we can board and traverse its stops
fn process_journey(
    data_storage: &DataStorage,
    state: &mut RaptorState,
    journey: &Journey,
    boarding_stop_id: i32,
    arrival_at_boarding: NaiveDateTime,
    previous_journey_id: Option<i32>,
    date: NaiveDate,
    round: usize,
    source_round: usize,
    time_limit: Option<NaiveDateTime>,
    target_stop_id: Option<i32>,
) {
    // Check if this is the last stop (can't board)
    if journey.is_last_stop(boarding_stop_id, true).unwrap_or(true) {
        return;
    }

    // Get departure time at this stop
    let Ok(departure_time) = journey.departure_at_of(boarding_stop_id, date) else {
        return;
    };

    // Check if we can catch this journey (respecting exchange time)
    let can_board = if let Some(prev_id) = previous_journey_id {
        let prev_journey = data_storage.journeys().find(prev_id);
        if let Some(prev) = prev_journey {
            // Check for through-service
            if has_through_service_for_journeys(data_storage, date, prev, journey, boarding_stop_id)
            {
                departure_time >= arrival_at_boarding
            } else {
                let exchange = get_exchange_time(
                    data_storage,
                    boarding_stop_id,
                    prev_id,
                    journey.id(),
                    departure_time,
                );
                let required_arrival =
                    add_minutes_to_date_time(arrival_at_boarding, exchange.into());
                departure_time >= required_arrival
            }
        } else {
            departure_time >= arrival_at_boarding
        }
    } else {
        departure_time >= arrival_at_boarding
    };

    if !can_board {
        return;
    }

    // Traverse the journey's stops from the boarding point
    // Only record stops that can be used as exchange points (or are the last stop)
    let mut found_boarding = false;
    for route_entry in journey.route().iter() {
        let stop_id = route_entry.stop_id();

        if stop_id == boarding_stop_id {
            found_boarding = true;
            continue;
        }

        if !found_boarding {
            continue;
        }

        // Only consider stops that can be used as exchange points or are the last stop
        let Some(stop) = data_storage.stops().find(stop_id) else {
            continue;
        };

        let is_last_stop = journey.is_last_stop(stop_id, false).unwrap_or(false);
        let is_target = target_stop_id == Some(stop_id);
        if !stop.can_be_used_as_exchange_point() && !is_last_stop && !is_target {
            continue;
        }

        // Get arrival time at this stop
        let Ok(arrival_time) =
            journey.arrival_at_of_with_origin(stop_id, date, true, boarding_stop_id)
        else {
            continue;
        };

        // Try to improve arrival at this stop
        state.try_improve(
            stop_id,
            arrival_time,
            Some(journey.id()),
            boarding_stop_id,
            boarding_stop_id,
            None,
            round,
            source_round,
            arrival_at_boarding,
            time_limit,
        );
    }
}

/// Check for through-service between two journeys at a stop
fn has_through_service_for_journeys(
    data_storage: &DataStorage,
    date: NaiveDate,
    journey_1: &Journey,
    journey_2: &Journey,
    stop_id: i32,
) -> bool {
    let through_service_bitfield = data_storage
        .bit_field_id_for_through_service_by_journey_id_stop_id()
        .get(&(
            (
                journey_1.legacy_id(),
                journey_1.administration().to_string(),
            ),
            (
                journey_2.legacy_id(),
                journey_2.administration().to_string(),
            ),
            stop_id,
        ));

    through_service_bitfield.is_some_and(|bf| {
        let bit_fields_by_day = data_storage.bit_fields_by_day().get(&date);
        bit_fields_by_day.is_some_and(|bfd| bfd.contains(bf))
    })
}

/// Process walking transfers from a stop
fn process_transfers(
    data_storage: &DataStorage,
    state: &mut RaptorState,
    stop_id: i32,
    round: usize,
    args: &RoutingAlgorithmArgs,
) {
    // Check if this stop can be used as an exchange point
    let Some(stop) = data_storage.stops().find(stop_id) else {
        return;
    };

    if !stop.can_be_used_as_exchange_point() {
        return;
    }

    // Get arrival time at this stop
    let Some(label) = state
        .labels_per_round
        .get(round)
        .and_then(|m| m.get(&stop_id))
    else {
        return;
    };
    let arrival_time = label.arrival_time;

    let time_limit = match args.mode() {
        RoutingAlgorithmMode::SolveFromDepartureStopToReachableArrivalStops => {
            Some(args.time_limit())
        }
        _ => None,
    };

    // Process walking connections
    if let Some(connections) = get_stop_connections(data_storage, stop_id) {
        for conn in connections {
            let to_stop_id = conn.stop_id_2();

            // Skip if target stop doesn't exist
            if data_storage.stops().find(to_stop_id).is_none() {
                continue;
            }

            let transfer_arrival = add_minutes_to_date_time(arrival_time, conn.duration().into());

            // Store transfer in the same round (transfers don't count as a new leg)
            state.try_improve(
                to_stop_id,
                transfer_arrival,
                None,
                stop_id,
                stop_id,
                Some(conn.duration()),
                round,
                round,
                arrival_time,
                time_limit,
            );
        }
    }
}

/// Reconstruct results from RAPTOR state
fn reconstruct_results(
    data_storage: &DataStorage,
    state: &RaptorState,
    departure_stop_id: i32,
    args: &RoutingAlgorithmArgs,
) -> FxHashMap<i32, RouteResult> {
    let mut results = FxHashMap::default();

    match args.mode() {
        RoutingAlgorithmMode::SolveFromDepartureStopToArrivalStop => {
            let target = args.arrival_stop_id();
            if state.best_arrival.contains_key(&target) {
                if let Some(route) =
                    reconstruct_route(data_storage, state, departure_stop_id, target)
                {
                    // Skip walking-only routes (not valid solutions)
                    if is_valid_route(&route) {
                        results.insert(target, route.to_route_result(data_storage));
                    }
                }
            }
        }
        RoutingAlgorithmMode::SolveFromDepartureStopToReachableArrivalStops => {
            for &stop_id in state.best_arrival.keys() {
                if stop_id == departure_stop_id {
                    continue;
                }
                if let Some(route) =
                    reconstruct_route(data_storage, state, departure_stop_id, stop_id)
                {
                    // Skip walking-only routes (not valid solutions)
                    if is_valid_route(&route) {
                        results.insert(stop_id, route.to_route_result(data_storage));
                    }
                }
            }
        }
    }

    results
}

/// Check if a route is valid for conversion to RouteResult.
/// A route is invalid if:
/// - It contains only walking sections (no transit)
/// - First section is walking but second section is also walking (or doesn't exist)
/// - Last section is walking but second-to-last is also walking (or doesn't exist)
fn is_valid_route(route: &Route) -> bool {
    let sections = route.sections();

    // Must have at least one section
    if sections.is_empty() {
        return false;
    }

    // Must have at least one transit section
    let has_transit = sections.iter().any(|s| s.journey_id().is_some());
    if !has_transit {
        return false;
    }

    // If first section is walking, second must exist and be transit
    if sections.first().unwrap().journey_id().is_none() {
        if sections.len() < 2 || sections[1].journey_id().is_none() {
            return false;
        }
    }

    // If last section is walking, second-to-last must exist and be transit
    if sections.last().unwrap().journey_id().is_none() {
        if sections.len() < 2 || sections[sections.len() - 2].journey_id().is_none() {
            return false;
        }
    }

    true
}

/// Reconstruct a route from the RAPTOR labels by walking backwards
///
/// Each label stores its source_round, which tells us exactly which round's label
/// was used to reach it. This ensures temporal consistency.
fn reconstruct_route(
    _data_storage: &DataStorage,
    state: &RaptorState,
    departure_stop_id: i32,
    target_stop_id: i32,
) -> Option<Route> {
    let mut sections = Vec::new();
    let mut visited_stops = FxHashSet::default();
    let mut current_stop = target_stop_id;

    // First, find the round where we reached the target with the best arrival
    let best_arrival = state.best_arrival.get(&target_stop_id)?;
    let mut current_round: Option<usize> = None;

    // Find the earliest round where we reached the target with the best arrival time
    for round in 0..state.labels_per_round.len() {
        if let Some(label) = state.labels_per_round[round].get(&target_stop_id) {
            if label.arrival_time == *best_arrival {
                current_round = Some(round);
                break;
            }
        }
    }

    let Some(mut round) = current_round else {
        return None;
    };

    // Walk backwards through the labels until we reach the departure stop
    while current_stop != departure_stop_id {
        // Find the label for current_stop at the current round
        let Some(label) = state
            .labels_per_round
            .get(round)
            .and_then(|m| m.get(&current_stop))
        else {
            return None;
        };

        // Prevent infinite loop - if we've already visited this stop, something is wrong
        if visited_stops.contains(&current_stop) {
            return None;
        }

        visited_stops.insert(current_stop);

        let section = RouteSection::new(
            label.journey_id,
            label.boarding_stop_id,
            current_stop,
            label.arrival_time,
            label.transfer_duration,
        );
        sections.push(section);

        // Move to the source stop at the source round
        current_stop = label.from_stop_id;
        round = label.source_round;
    }

    if sections.is_empty() {
        return None;
    }

    visited_stops.insert(departure_stop_id);
    sections.reverse();

    Some(Route::new(sections, visited_stops))
}
