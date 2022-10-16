//! This module implements grid-filling using a backtracking search algorithm that's mostly based on
//! recommendations in "Adaptive Strategies for Solving Constraint Satisfaction Problems" by
//! Thanasis Balafoutis. In addition to maintaining arc consistency using AC-3 and ordering
//! variables with a variant of the `dom/wdeg` heuristic, we incorporate Balafoutis's "adaptive
//! branching" concept and randomized restarts.

use float_ord::FloatOrd;
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use smallvec::SmallVec;
use std::collections::HashSet;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::time::{Duration, Instant};

use crate::arc_consistency::{
    establish_arc_consistency, ArcConsistencyAdapter, ArcConsistencyFailure, ArcConsistencySuccess,
};
use crate::grid_config::{Choice, Crossing, GridConfig, SlotId};
use crate::util::{build_glyph_counts_by_cell, GlyphCountsByCell};
use crate::word_list::WordId;
use crate::{CHECK_INVARIANTS, MAX_SLOT_COUNT};

/// If the previously-attempted slot is within this distance of the "best" (lowest-priority-value)
/// slot, we should stick with the previous one instead of switching (per Balafoutis).
pub const ADAPTIVE_BRANCHING_THRESHOLD: f32 = 0.15;

/// How many times should we loop before checking whether we've passed our deadline or received
/// a message on our abort channel?
pub const INTERRUPT_FREQUENCY: usize = 10;

/// How much do we decrease the weight of each crossing every time we wipe out a domain?
/// The lower this is, the more we prioritize recent information over older information.
pub const WEIGHT_AGE_FACTOR: f32 = 0.99;

/// How do we weigh the highest-ranked N slots when choosing which one to fill next?
pub const RANDOM_SLOT_WEIGHTS: [u8; 3] = [4, 2, 1];

/// How do we weigh the highest-ranked N words when choosing a word for a given slot?
pub const RANDOM_WORD_WEIGHTS: [u8; 3] = [4, 2, 1];

/// How much do we increase the backtrack limit when retrying?
pub const RETRY_GROWTH_FACTOR: f32 = 1.1;

/// A struct tracking stats about the filling process.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct Statistics {
    pub states: usize,
    pub backtracks: usize,
    pub restricted_branchings: usize,
    pub retries: usize,
    pub total_time: Duration,
    pub try_time: Duration,
    pub initial_arc_consistency_time: Duration,
    pub choice_arc_consistency_time: Duration,
    pub elimination_arc_consistency_time: Duration,
}

/// A struct tracking the live state of a single slot during filling.
#[derive(Clone)]
pub struct Slot {
    /// Properties duplicated from `SlotConfig` for convenience.
    id: SlotId,
    length: usize,

    /// Record of which options from `slot_options` have been eliminated from this slot, stored as
    /// a Vec indexed by WordId:
    /// * `Some(Some(id))` means "this option has been eliminated by the choice in slot `id`"
    /// * `Some(None)` means "this option has been eliminated regardless of any choices"
    /// * `None` means "this option has not been eliminated (or was never available)"
    eliminations: Vec<Option<Option<SlotId>>>,

    /// To enable us to quickly validate crossing slots, we maintain a count of the number of
    /// instances of each glyph in each cell in our remaining options.
    glyph_counts_by_cell: GlyphCountsByCell,

    /// How many options are still available for this slot? Note that this is based on the
    /// `slot_options` from GridConfig, not the `words` from WordList, since the latter also
    /// includes hidden words that aren't available for this fill attempt.
    remaining_option_count: usize,

    // The word id explicitly chosen for this slot during the fill process (or as part of the input
    // to the fill process), if there is one. This takes precedence over `eliminations`,
    // `glyph_counts_by_cell`, and `remaining_option_count`, which will be kept in the state they
    // were in before the choice was made.
    fixed_word_id: Option<WordId>,
    fixed_glyph_counts_by_cell: Option<GlyphCountsByCell>,
}

impl Debug for Slot {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("id", &self.id)
            .field(
                "eliminations",
                &format!(
                    "({} eliminations)",
                    self.eliminations.iter().flatten().count()
                ),
            )
            .field("remaining_option_count", &self.remaining_option_count)
            .field("fixed_word_id", &self.fixed_word_id)
            .finish()
    }
}

impl Slot {
    /// Record that a word is unavailable for a slot, along with the slot id responsible so that we
    /// can roll it back if we backtrack the relevant decision.
    fn add_elimination(
        &mut self,
        config: &GridConfig,
        word_id: WordId,
        blamed_slot_id: Option<SlotId>,
    ) {
        if CHECK_INVARIANTS
            && (self.fixed_word_id.is_some() || self.fixed_glyph_counts_by_cell.is_some())
        {
            panic!("Editing eliminations for a fixed slot?");
        }

        self.eliminations[word_id] = Some(blamed_slot_id);
        self.remaining_option_count -= 1;

        let word = &config.word_list.words[self.length][word_id];
        for (cell_idx, &glyph) in word.glyphs.iter().enumerate() {
            self.glyph_counts_by_cell[cell_idx][glyph] -= 1;
        }
    }

    /// Record that a word is now available again for this slot.
    fn remove_elimination(&mut self, config: &GridConfig, word_id: WordId) {
        if CHECK_INVARIANTS
            && (self.fixed_word_id.is_some() || self.fixed_glyph_counts_by_cell.is_some())
        {
            panic!("Editing eliminations for a fixed slot?");
        }

        self.eliminations[word_id] = None;
        self.remaining_option_count += 1;

        let word = &config.word_list.words[self.length][word_id];
        for (cell_idx, &glyph) in word.glyphs.iter().enumerate() {
            self.glyph_counts_by_cell[cell_idx][glyph] += 1;
        }
    }

    /// Remove all eliminations that were created because of the last choice in the given slot.
    fn clear_eliminations(&mut self, config: &GridConfig, slot_id: SlotId) {
        for word_id in 0..self.eliminations.len() {
            if self.eliminations[word_id] == Some(Some(slot_id)) {
                self.remove_elimination(config, word_id);
            }
        }
    }

    /// Record a choice, shadowing the existing eliminations, glyph counts, etc.
    fn choose_word(&mut self, config: &GridConfig, word_id: WordId) {
        self.fixed_word_id = Some(word_id);
        self.fixed_glyph_counts_by_cell = Some(build_glyph_counts_by_cell(
            config.word_list,
            self.length,
            &[word_id],
        ));
    }

    /// Clear a choice. Since we only ever backtrack linearly, the previously-stored eliminations,
    /// glyph counts, etc., should still be correct.
    fn clear_choice(&mut self) {
        self.fixed_word_id = None;
        self.fixed_glyph_counts_by_cell = None;
    }

    /// Build a Choice struct representing this slot's single remaining word.
    fn get_choice(&self, config: &GridConfig) -> Option<Choice> {
        self.fixed_word_id
            .map(|word_id| Choice {
                slot_id: self.id,
                word_id,
            })
            .or_else(|| {
                if self.remaining_option_count != 1 {
                    None
                } else {
                    let word_id = config.slot_options[self.id]
                        .iter()
                        .find(|&&word_id| self.eliminations[word_id].is_none());

                    word_id.map(|&word_id| Choice {
                        slot_id: self.id,
                        word_id,
                    })
                }
            })
    }
}

/// Calculate the weight of a slot as defined in the `wdeg` heuristic, which is the sum of the
/// weights of any crossings it has where the other slot is still undetermined.
fn calculate_slot_weight(
    config: &GridConfig,
    slots: &[Slot],
    crossing_weights: &[f32],
    slot_id: SlotId,
) -> f32 {
    config.slot_configs[slot_id]
        .crossings
        .iter()
        .map(|crossing| match crossing {
            Some(Crossing {
                other_slot_id,
                crossing_id,
                ..
            }) => {
                if slots[*other_slot_id].remaining_option_count > 1 {
                    crossing_weights[*crossing_id]
                } else {
                    0.0
                }
            }
            None => 0.0,
        })
        .sum()
}

/// Calculate the weights of all slots as defined in the `wdeg` heuristic.
fn calculate_slot_weights(
    config: &GridConfig,
    slots: &[Slot],
    crossing_weights: &[f32],
) -> Vec<f32> {
    (0..slots.len())
        .map(|slot_id| calculate_slot_weight(config, slots, crossing_weights, slot_id))
        .collect()
}

/// Calculate the priority of a slot, a measurement of how good a candidate it is to fill
/// next (where lower is better). This is an implementation of a version of the `dom/wdeg`
/// heuristic, although the specific meaning of the "weight" of each crossing depends on
/// our implementation of arc consistency.
fn calculate_slot_priority(slots: &[Slot], slot_weights: &[f32], slot_id: SlotId) -> f32 {
    (slots[slot_id].remaining_option_count as f32) / slot_weights[slot_id]
}

#[derive(Debug)]
enum ArcConsistencyMode {
    Initial,
    Choice(Choice),
    Elimination(Choice, Option<SlotId>),
}

/// Within the context of a fill attempt, either establish initial arc consistency, propagate the
/// impact of a choice, or propagate the impact of an elimination. Also update crossing weights
/// if it turns out to be impossible to achieve consistency (a "domain wipeout").
fn maintain_arc_consistency(
    config: &GridConfig,
    slots: &mut [Slot],
    crossing_weights: &mut [f32],
    slot_weights: &[f32],
    mode: ArcConsistencyMode,
    time: &mut Duration,
) -> bool {
    let start = Instant::now();

    // First, if we're testing a choice or elimination, update the relevant state provisionally.
    match &mode {
        ArcConsistencyMode::Choice(choice) => {
            slots[choice.slot_id].choose_word(config, choice.word_id);
        }

        ArcConsistencyMode::Elimination(choice, blamed_slot_id) => {
            slots[choice.slot_id].add_elimination(config, choice.word_id, *blamed_slot_id);
        }

        _ => {}
    };

    let remaining_option_counts = slots
        .iter()
        .map(|slot| {
            if slot.fixed_word_id.is_some() {
                1
            } else {
                slot.remaining_option_count
            }
        })
        .collect::<Vec<_>>();

    let fixed_slots: Vec<bool> = if matches!(mode, ArcConsistencyMode::Initial) {
        // When establishing initial consistency, only slots whose contents were provided verbatim
        // should be considered fixed -- other slots might happen to only have one available option,
        // but then that option could be ruled out by crossings.
        slots
            .iter()
            .map(|slot| slot.fixed_word_id.is_some())
            .collect()
    } else {
        // When maintaining consistency later on, we can treat all slots with exactly one option as
        // fixed, because all of their crossings will already have been pruned to only compatible
        // options and we'll already have removed any possible dupe-rule violations from the rest of
        // the grid. Also if we're evaluating a choice we'll treat that choice's slot as fixed.
        slots
            .iter()
            .map(|slot| remaining_option_counts[slot.id] == 1)
            .collect()
    };

    struct Adapter<'a> {
        config: &'a GridConfig<'a>,
        slots: &'a mut [Slot],
    }
    impl<'a> ArcConsistencyAdapter for Adapter<'a> {
        fn is_word_eliminated(&self, slot_id: SlotId, word_id: WordId) -> bool {
            self.slots[slot_id].eliminations[word_id].is_some()
        }
        fn get_glyph_counts(&self, slot_id: SlotId) -> GlyphCountsByCell {
            self.slots[slot_id]
                .fixed_glyph_counts_by_cell
                .clone()
                .unwrap_or_else(|| self.slots[slot_id].glyph_counts_by_cell.clone())
        }
        fn get_single_option(
            &self,
            slot_id: SlotId,
            eliminations: &HashSet<WordId>,
        ) -> Option<WordId> {
            self.slots[slot_id].fixed_word_id.or_else(|| {
                self.config.slot_options[slot_id]
                    .iter()
                    .find(|word_id| !eliminations.contains(word_id))
                    .cloned()
            })
        }
    }

    let starting_slot_id = match &mode {
        ArcConsistencyMode::Initial => None,
        ArcConsistencyMode::Choice(choice) => Some(choice.slot_id),
        ArcConsistencyMode::Elimination(choice, _) => Some(choice.slot_id),
    };

    let blamed_slot_id = match &mode {
        ArcConsistencyMode::Initial => None,
        ArcConsistencyMode::Choice(choice) => Some(choice.slot_id),
        ArcConsistencyMode::Elimination(_, blamed_slot_id) => *blamed_slot_id,
    };

    let success = match establish_arc_consistency(
        config,
        &Adapter { config, slots },
        &remaining_option_counts,
        crossing_weights,
        slot_weights,
        &fixed_slots,
        starting_slot_id,
    ) {
        // If we succeeded, we just need to apply the new eliminations to each slot and we're done.
        Ok(ArcConsistencySuccess { eliminations }) => {
            for (slot_id, eliminations) in eliminations.into_iter().enumerate() {
                for word_id in eliminations {
                    slots[slot_id].add_elimination(config, word_id, blamed_slot_id);
                }
            }

            true
        }

        // If we failed, we need to undo any provisional changes we made above and update our
        // crossing weights to reflect the causes of the failure.
        Err(ArcConsistencyFailure { weight_updates }) => {
            match &mode {
                ArcConsistencyMode::Choice(choice) => {
                    slots[choice.slot_id].clear_choice();
                }

                ArcConsistencyMode::Elimination(choice, ..) => {
                    slots[choice.slot_id].remove_elimination(config, choice.word_id);
                }

                _ => {}
            };

            for (slot_id, weight) in crossing_weights.iter_mut().enumerate() {
                *weight = 1.0
                    + ((*weight - 1.0) * WEIGHT_AGE_FACTOR)
                    + weight_updates.get(&slot_id).unwrap_or(&0.0);
            }

            false
        }
    };

    *time += Instant::now() - start;

    success
}

/// Identify the next slot we should try to fill, based on a combination of the `dom/wdeg` priority
/// algorithm with an "adaptive branching" strategy that stays on the same slot if the "best" one
/// is close enough in priority.
fn choose_next_slot(
    slots: &[Slot],
    slot_weights: &[f32],
    last_slot_id: Option<SlotId>,
    rng: &mut SmallRng,
    dist: &WeightedIndex<u8>,
    statistics: &mut Statistics,
) -> Option<SlotId> {
    let mut best_slot_priority: Option<f32> = None;
    let mut last_slot_priority: Option<f32> = None;

    let mut sorted_slot_ids: Vec<_> = (0..slots.len())
        .filter(|&slot_id| {
            // If the slot only has one option, whether it was chosen explicitly or implicitly, we can
            // just leave it alone.
            slots[slot_id].fixed_word_id.is_none() && slots[slot_id].remaining_option_count > 1
        })
        .collect();

    // If there are no slots left to choose from, we're done.
    if sorted_slot_ids.is_empty() {
        return None;
    }

    // Otherwise, sort the remaining slots by priority.
    sorted_slot_ids.sort_by_cached_key(|&slot_id| {
        let priority = calculate_slot_priority(slots, slot_weights, slot_id);

        if best_slot_priority.map_or(true, |best_priority| best_priority > priority) {
            best_slot_priority = Some(priority);
        }

        if last_slot_id.map_or(false, |last_id| last_id == slot_id) {
            last_slot_priority = Some(priority);
        }

        FloatOrd(priority)
    });

    // If the best slot isn't that much better than the one we're on, stay with the one we're on.
    if let Some(best_slot_priority) = best_slot_priority {
        if let (Some(last_slot_id), Some(last_slot_priority)) = (last_slot_id, last_slot_priority) {
            if (last_slot_priority - best_slot_priority) < ADAPTIVE_BRANCHING_THRESHOLD {
                statistics.restricted_branchings += 1;
                return Some(last_slot_id);
            }
        }
    }

    // Otherwise, take one of the best few slots at random.
    Some(sorted_slot_ids[dist.sample(rng).min(sorted_slot_ids.len() - 1)])
}

/// A struct representing the results of a fill operation.
#[derive(Debug)]
#[allow(dead_code)]
pub struct FillSuccess {
    pub statistics: Statistics,
    pub choices: Vec<Choice>,
}

#[derive(Debug)]
pub enum FillFailure {
    HardFailure,
    Timeout,
    Abort,
    ExceededBacktrackLimit(usize),
}

/// Search for a valid fill for the given grid, bailing out if we reach the deadline or the
/// specified number of backtracks. We receive some state as arguments that can be shared between
/// multiple retries of the same overall search attempt.
pub fn find_fill_for_seed(
    config: &GridConfig,
    slots: &SmallVec<[Slot; MAX_SLOT_COUNT]>,
    deadline: Option<Instant>,
    max_backtracks: usize,
    rng_seed: u64,
    crossing_weights: &mut [f32],
) -> Result<FillSuccess, FillFailure> {
    let start = Instant::now();
    let mut rng: SmallRng = SeedableRng::seed_from_u64(rng_seed);
    let mut statistics = Statistics::default();

    let mut slots: SmallVec<[Slot; MAX_SLOT_COUNT]> = (*slots).clone();

    // Track slot choices made so far in the process.
    let mut choices: Vec<Choice> = Vec::with_capacity(config.slot_configs.len());

    let mut last_slot_id: Option<SlotId> = None;
    let mut last_starting_word_idx: Option<usize> = None;

    let slot_dist = WeightedIndex::new(RANDOM_SLOT_WEIGHTS).unwrap();
    let word_dist = WeightedIndex::new(RANDOM_WORD_WEIGHTS).unwrap();

    // Enter the main loop:
    // * Choose an option for a slot and try to propagate constraints for it. If we succeed, we keep
    //   the choice and continue the loop.
    // * If we failed to choose an option, record that the option is unavailable and try to
    //   propagate constraints for that. If we succeed, we continue the loop, most likely trying to
    //   pick another option for the same slot but also potentially changing slots.
    // * If we also failed to propagate constraints with the chosen option being *un*available, it
    //   means the previous choice we made is untenable. Try to undo it and propagate the
    //   information that *that* choice is unavailable. Repeat until we reach a viable state, or
    //   abandon the fill attempt if we can't.
    loop {
        statistics.states += 1;

        if statistics.states % INTERRUPT_FREQUENCY == 0 {
            if let Some(deadline) = deadline {
                if Instant::now() > deadline {
                    return Err(FillFailure::Timeout);
                }
            }
            if let Some(abort_rx) = config.abort_rx {
                if abort_rx.try_recv().is_ok() {
                    return Err(FillFailure::Abort);
                }
            }
        }

        // Choose which slot to try to fill.
        let slot_weights = calculate_slot_weights(config, &slots, crossing_weights);
        let slot_id = match choose_next_slot(
            &slots,
            &slot_weights,
            last_slot_id,
            &mut rng,
            &slot_dist,
            &mut statistics,
        ) {
            Some(slot_id) => slot_id,
            None => {
                // If there are no more slots to fill, it means we're done.
                statistics.total_time = start.elapsed();

                // We need to build a `choices` array that includes both choices we made explicitly
                // and ones that were made implicitly by maintaining arc consistency.
                let choices = slots
                    .into_iter()
                    .map(|slot| {
                        slot.get_choice(config)
                            .expect("Failed to identify single choice for slot")
                    })
                    .collect();

                return Ok(FillSuccess {
                    statistics,
                    choices,
                });
            }
        };

        // If we're still on the same slot as last time, start from where we left off instead of
        // rechecking previously-evaluated words.
        let starting_word_idx: usize = if Some(slot_id) == last_slot_id {
            last_starting_word_idx.unwrap_or(0)
        } else {
            0
        };

        // Take as many available candidate words as we have weights in `RANDOM_WORD_WEIGHTS`.
        let word_candidates: Vec<(usize, &WordId)> = config.slot_options[slot_id]
            .iter()
            .enumerate()
            .skip(starting_word_idx)
            .filter(|&(_, &word_id)| slots[slot_id].eliminations[word_id].is_none())
            .take(RANDOM_WORD_WEIGHTS.len())
            .collect();

        if word_candidates.is_empty() {
            panic!("Unable to find option for slot {:?}", slots[slot_id]);
        }

        // Choose one of the candidates at (weighted) random.
        let (_, &word_id) =
            word_candidates[word_dist.sample(&mut rng).min(word_candidates.len() - 1)];

        // Record our position so we can pick up where we left off if needed, using the first
        // candidate index to make sure we don't skip any words.
        last_slot_id = Some(slot_id);
        last_starting_word_idx = Some(word_candidates[0].0);

        let choice = Choice { slot_id, word_id };

        // Try to propagate the implications of making this choice to the rest of the grid.
        if maintain_arc_consistency(
            config,
            &mut slots,
            crossing_weights,
            &slot_weights,
            ArcConsistencyMode::Choice(choice.clone()),
            &mut statistics.choice_arc_consistency_time,
        ) {
            // If we successfully propagated constraints for this choice, we can record it and
            // move on to the next slot.
            choices.push(choice);
            continue;
        }

        // Otherwise, we can rule this option out. If we can successfully propagate the implications
        // of that elimination, we can move on to the next slot; otherwise, we need to keep
        // backtracking until we find a choice we can successfully propagate the reversal of.
        let mut undoing_choice = choice;
        loop {
            statistics.backtracks += 1;

            if maintain_arc_consistency(
                config,
                &mut slots,
                crossing_weights,
                &slot_weights,
                ArcConsistencyMode::Elimination(
                    undoing_choice.clone(),
                    choices.last().map(|choice| choice.slot_id),
                ),
                &mut statistics.elimination_arc_consistency_time,
            ) {
                // If we successfully propagated constraints for this elimination, we're done
                // backtracking and can return to the top-level loop.
                break;
            }

            // If we didn't, it means the previous choice is also no longer viable, because we've
            // now proven that given all previous choices, neither `slot_id = word_id`
            // nor `slot_id != word_id` are possible. We should undo the impact of that
            // choice and then continue the backtracking loop to see if it's possible to propagate
            // the opposite of the choice.
            undoing_choice = match choices.pop() {
                Some(choice) => choice,
                None => {
                    // If there are no previous choices, we've now proven that the whole grid is
                    // unsolvable.
                    return Err(FillFailure::HardFailure);
                }
            };

            slots[undoing_choice.slot_id].clear_choice();

            for slot in slots.iter_mut() {
                if slot.id != undoing_choice.slot_id && slot.fixed_word_id.is_none() {
                    slot.clear_eliminations(config, undoing_choice.slot_id);
                }
            }

            // If we've exceeded our backtrack limit, restart the fill process with a new seed.
            if statistics.backtracks > max_backtracks {
                return Err(FillFailure::ExceededBacktrackLimit(statistics.backtracks));
            }

            // Our cached position in the last slot's option list is now invalid.
            last_slot_id = None;
            last_starting_word_idx = None;
        }
    }
}

/// Search for a valid fill for the given grid, if one can be found within the given amount of time.
#[allow(dead_code)]
pub fn find_fill(
    config: &GridConfig,
    timeout: Option<Duration>,
) -> Result<FillSuccess, FillFailure> {
    let start = Instant::now();
    let deadline = timeout.map(|timeout| start + timeout);

    // Create basic Slot structs for the grid, which we can copy for each retry instead of having
    // to regenerate from scratch.
    let mut slots: SmallVec<[Slot; MAX_SLOT_COUNT]> = config
        .slot_configs
        .iter()
        .map(|slot_config| {
            let glyph_counts_by_cell = build_glyph_counts_by_cell(
                config.word_list,
                slot_config.length,
                &config.slot_options[slot_config.id],
            );

            let is_fixed = slot_config
                .complete_fill(config.fill, config.width)
                .is_some();

            Slot {
                id: slot_config.id,
                length: slot_config.length,
                eliminations: vec![None; config.word_list.words[slot_config.length].len()],
                remaining_option_count: config.slot_options[slot_config.id].len(),
                fixed_word_id: if is_fixed {
                    if config.slot_options[slot_config.id].len() != 1 {
                        panic!("Slot has complete fill but option count != 1?");
                    } else {
                        Some(config.slot_options[slot_config.id][0])
                    }
                } else {
                    None
                },
                fixed_glyph_counts_by_cell: if is_fixed {
                    Some(glyph_counts_by_cell.clone())
                } else {
                    None
                },
                glyph_counts_by_cell,
            }
        })
        .collect();

    // Start tracking weights representing how problematic each crossing is in the grid. These are
    // shared between retries so that we can learn from each one.
    let mut crossing_weights: Vec<f32> = (0..config.crossing_count).map(|_| 1.0).collect();

    // Establish initial arc consistency (including dupe-checking). If we can't even do that, we're
    // obviously not going to be able to find a fill.
    let slot_weights = calculate_slot_weights(config, &slots, &crossing_weights);
    let mut initial_arc_consistency_time = Duration::default();
    if !maintain_arc_consistency(
        config,
        &mut slots,
        &mut crossing_weights,
        &slot_weights,
        ArcConsistencyMode::Initial,
        &mut initial_arc_consistency_time,
    ) {
        return Err(FillFailure::HardFailure);
    }

    // We cap the number of backtracks for each retry so that we don't get hung up for too long on a
    // bad starting point.
    let mut max_backtracks: usize = 500;

    // Now keep trying to fill the grid until we either succeed or run out of time. Each attempt has
    // a slightly larger `max_backtracks` value in addition to having a new RNG seed.
    for retry_num in 0.. {
        match find_fill_for_seed(
            config,
            &slots,
            deadline,
            max_backtracks,
            retry_num,
            &mut crossing_weights,
        ) {
            Ok(mut result) => {
                result.statistics.retries = retry_num as usize;
                result.statistics.try_time = result.statistics.total_time;
                result.statistics.total_time = start.elapsed();
                result.statistics.initial_arc_consistency_time = initial_arc_consistency_time;
                return Ok(result);
            }
            Err(FillFailure::ExceededBacktrackLimit(_backtrack_count)) => {
                // Ensure that we always increase `max_backtracks` by at least 1.
                max_backtracks = (max_backtracks + 1)
                    .max((max_backtracks as f32 * RETRY_GROWTH_FACTOR) as usize);
            }
            other_error => {
                return other_error;
            }
        }
    }

    unreachable!();
}

#[cfg(test)]
mod tests {
    use crate::backtracking_search::find_fill;
    use crate::grid_config::{
        generate_grid_config_from_template_string, render_grid, OwnedGridConfig,
    };
    use crate::word_list::tests::dictionary_path;
    use crate::word_list::WordList;

    fn load_word_list(max_length: usize) -> WordList {
        WordList::from_dict_file(&dictionary_path(), Some(max_length), Some(5)).unwrap()
    }

    fn generate_config(template: &str) -> OwnedGridConfig {
        let template = template.trim();
        let width = template.lines().map(|line| line.len()).max().unwrap();
        let height = template.lines().count();
        generate_grid_config_from_template_string(load_word_list(width.max(height)), template, 40.0)
    }

    #[test]
    fn test_find_fill_for_3x3_square() {
        let grid_config = generate_config(
            "
            ...
            ...
            ...
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_5x5_square() {
        let grid_config = generate_config(
            "
            .....
            .....
            .....
            .....
            .....
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_6x6_square() {
        let grid_config = generate_config(
            "
            ......
            ......
            ......
            ......
            ......
            ......
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_empty_7x7_template() {
        let grid_config = generate_config(
            "
            #...###
            #....##
            .......
            .......
            .......
            ##....#
            ###...#
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_partially_populated_7x7_template() {
        let grid_config = generate_config(
            "
            #..s###
            #..i.##
            ...m...
            .......
            .......
            ##....#
            ###...#
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_dupe_prevention_doesnt_affect_prefilled_entries() {
        let grid_config = generate_config(
            "
            #..p###
            #..a.##
            ...r...
            partiii
            ...i...
            ##.e..#
            ###s..#
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
    }

    #[test]
    fn test_fill_fails_gracefully() {
        let grid_config = generate_config(
            "
            #..x###
            #....##
            ......x
            ......x
            ......x
            ##....#
            ###..x#
            ",
        );

        find_fill(&grid_config.to_config_ref(), None).expect_err("Found an impossible fill??");
    }

    #[test]
    fn test_find_fill_for_empty_15x15_themed_template() {
        let grid_config = generate_config(
            "
            ....#.....#....
            ....#.....#....
            ...............
            ......##.......
            ###.....#......
            ............###
            .....#.....#...
            ....#.....#....
            ...#.....#.....
            ###............
            ......#.....###
            .......##......
            ...............
            ....#.....#....
            ....#.....#....
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_empty_15x15_cryptic_template() {
        let grid_config = generate_config(
            "
            ....#....#....#
            .#.#.#.#.#.#.#.
            ...............
            .#.#.#.#.#.#.#.
            ...............
            ##.#.#.#.###.#.
            ...............
            .###.#####.###.
            ...............
            .#.###.#.#.#.##
            ...............
            .#.#.#.#.#.#.#.
            ...............
            .#.#.#.#.#.#.#.
            #....#....#....
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_empty_15x15_themeless_template() {
        let grid_config = generate_config(
            "
            ..........#....
            ..........#....
            ..........#....
            ...#...#.......
            ....###........
            .........#.....
            ###.......#....
            ...#.......#...
            ....#.......###
            .....#.........
            ........###....
            .......#...#...
            ....#..........
            ....#..........
            ....#..........
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }

    #[test]
    fn test_find_fill_for_partially_populated_15x15_themeless_template() {
        let grid_config = generate_config(
            "
            .......##......
            admirers#......
            .......t.......
            .....#.i...#...
            ....#..c..#....
            ...#...k.#.....
            ###....y......#
            ##.....f.....##
            #......i....###
            .....#.n...#...
            ....#..g..#....
            ...#...e.#.....
            .......r.......
            ......#s.......
            ......##.......
            ",
        );

        let result = find_fill(&grid_config.to_config_ref(), None).expect("Failed to find a fill");

        println!("{:?}", result.statistics);
        println!(
            "{}",
            render_grid(&grid_config.to_config_ref(), &result.choices)
        );
    }
}
