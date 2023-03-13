//! This module contains a crossword-specific implementation of the AC-3 algorithm for establishing
//! and maintaining arc consistency. For our purposes, a grid is arc-consistent when:
//!
//! - We've removed all options that rely on letters that are unavailable in crossing slots. For
//!   example, if 1D doesn't have any options starting with the letter A, we want to remove any
//!   options for 1A that start with the letter A.
//!
//! - For each slot that has been reduced to one option, we've removed all options from other slots
//!   that are incompatible because of dupe rules (either because they are identical to that option
//!   or share a specified number of chars in a row).
//!
//! We keep applying these rules until no more eliminations are possible.

use float_ord::FloatOrd;
use smallvec::{smallvec, SmallVec};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::mem;

use crate::grid_config::{Crossing, CrossingId, GridConfig, SlotId};
use crate::util::{build_glyph_counts_by_cell, GlyphCountsByCell};
use crate::word_list::WordId;
use crate::{MAX_SLOT_COUNT, MAX_SLOT_LENGTH};

/// Interface that needs to be implemented by callers to `establish_arc_consistency` to provide
/// context about the state of the grid before this call.
pub trait ArcConsistencyAdapter {
    /// Was this word already eliminated for this slot?
    fn is_word_eliminated(&self, slot_id: SlotId, word_id: WordId) -> bool;

    /// What were the glyph counts for this slot before this call? (See `util.rs` for context about
    /// glyph counts.)
    fn get_glyph_counts(&self, slot_id: SlotId) -> GlyphCountsByCell;

    /// What is the single remaining option for this slot, given eliminations made both before and
    /// during the arc-consistency process (with the latter provided as a param)?
    fn get_single_option(&self, slot_id: SlotId, eliminations: &HashSet<WordId>) -> Option<WordId>;
}

/// Result from a successful call to `establish_arc_consistency`, reflecting which words need to be
/// eliminated from each slot.
#[derive(Debug)]
pub struct ArcConsistencySuccess {
    pub eliminations: Vec<HashSet<WordId>>,
}

/// Result from a failed call to `establish_arc_consistency`, reflecting how responsible each
/// crossing was for the domain wipeout.
#[derive(Debug)]
pub struct ArcConsistencyFailure {
    pub weight_updates: HashMap<CrossingId, f32>,
}

/// Result from a call to `establish_arc_consistency`.
pub type ArcConsistencyResult = Result<ArcConsistencySuccess, ArcConsistencyFailure>;

/// Struct tracking the state of a given slot during the process of establishing arc consistency.
struct ArcConsistencySlotState {
    /// The id of the underlying slot; this is an index into various slices passed into
    /// `establish_arc_consistency`.
    slot_id: SlotId,

    /// The set of words eliminated as part of this process. This doesn't include anything that was
    /// already eliminated beforehand.
    eliminations: HashSet<WordId>,

    /// A map from each cell index to the number of eliminations its crossing has added to this slot
    /// so far. This is used to calculate new crossing weights if the propagation process fails.
    blame_counts: SmallVec<[usize; MAX_SLOT_LENGTH]>,

    /// The live count of words available, taking both global and local eliminations into account.
    option_count: usize,

    /// Live glyph counts for this slot, as retrieved lazily from the adapter and then updated in
    /// place.
    glyph_counts_by_cell: Option<GlyphCountsByCell>,

    /// A set of cell indices that we need to propagate *outward* from, removing any incompatible
    /// options from the crossing entry.
    queued_cell_idxs: Option<SmallVec<[usize; MAX_SLOT_LENGTH]>>,

    /// Do we need to do singleton propagation (e.g., uniqueness checks) from this slot? This can
    /// only be true if the slot has exactly one entry and we've never done this propagation from
    /// it.
    needs_singleton_propagation: bool,
}

impl ArcConsistencySlotState {
    /// Get the current glyph counts for this slot, lazily fetching initial values from the adapter
    /// if needed.
    #[inline(always)]
    #[allow(clippy::inline_always)]
    fn get_glyph_counts<Adapter: ArcConsistencyAdapter>(
        &mut self,
        adapter: &Adapter,
    ) -> &mut GlyphCountsByCell {
        if self.glyph_counts_by_cell.is_none() {
            self.glyph_counts_by_cell = Some(adapter.get_glyph_counts(self.slot_id));
        }
        self.glyph_counts_by_cell.as_mut().unwrap()
    }
}

/// Determine which eliminations are needed to bring the grid into an arc-consistent state.
/// If it's impossible to make the grid consistent, return weight values reflecting which
/// constraints are responsible for the failure (sort of).
#[allow(clippy::too_many_lines)]
pub fn establish_arc_consistency<Adapter: ArcConsistencyAdapter>(
    config: &GridConfig,
    adapter: &Adapter,

    // For each slot, how many options are available at the beginning of the process?
    initial_option_counts: &[usize],

    // For each crossing, what "weight" value has been assigned to it so far? A higher weight means
    // the crossing has been more difficult to satisfy.
    crossing_weights: &[f32],

    // For each slot, what "weight" value has been assigned to it so far? A higher weight means
    // the slot's currently-unfilled crossings have been more difficult to satisfy.
    slot_weights: &[f32],

    // For each slot, should its value be considered "fixed", meaning that its single option can't
    // be eliminated? This is true of slots that are prefilled and slots that we've made a choice
    // for during a fill process, but not slots that just happen to have been reduced to a
    // single option by previous constraint propagation.
    fixed_slots: &[bool],

    // If this param has a value, it means we can assume the grid was previously arc consistent and
    // then this one slot had its domain reduced, and our job is just to propagate the implications
    // of that. If it doesn't have a value, it means we need to establish global arc consistency by
    // checking every slot in the grid.
    evaluating_slot: Option<SlotId>,
) -> ArcConsistencyResult {
    let mut slot_states: SmallVec<[ArcConsistencySlotState; MAX_SLOT_COUNT]> = config
        .slot_configs
        .iter()
        .map(|slot_config| ArcConsistencySlotState {
            slot_id: slot_config.id,
            eliminations: HashSet::with_capacity(initial_option_counts[slot_config.id] / 2),
            blame_counts: smallvec![0; slot_config.length],
            option_count: initial_option_counts[slot_config.id],
            glyph_counts_by_cell: None,
            queued_cell_idxs: None,
            needs_singleton_propagation: false,
        })
        .collect();

    // If we were given an `evaluating_slot`, we can assume that the rest of the grid is fully
    // arc-consistent and start by just queueing the cells of this slot. Otherwise, we want to
    // examine the whole grid, except slots that are fixed already.
    let initial_slot_ids: Vec<SlotId> = evaluating_slot.map_or_else(
        || (0..config.slot_configs.len()).collect(),
        |evaluating_slot| vec![evaluating_slot],
    );
    for slot_id in initial_slot_ids {
        // Queue all cells that have a crossing with a non-fixed slot.
        slot_states[slot_id].queued_cell_idxs = Some(
            config.slot_configs[slot_id]
                .crossings
                .iter()
                .enumerate()
                .filter(|(_, crossing_opt)| {
                    if let Some(crossing) = crossing_opt {
                        !fixed_slots[crossing.other_slot_id]
                    } else {
                        false
                    }
                })
                .map(|(cell_idx, _)| cell_idx)
                .collect(),
        );

        // If this slot has a single option, we also want to remove dupes from other slots.
        if slot_states[slot_id].option_count == 1 {
            slot_states[slot_id].needs_singleton_propagation = true;
        }
    }

    // Whenever we eliminate an option from a slot, we need to do some bookkeeping and potentially
    // enqueue cells from that slot for further propagation.
    let eliminate_word = |slot_states: &mut [ArcConsistencySlotState],
                          slot_id: SlotId,
                          word_id: WordId,
                          blamed_cell_idx: Option<usize>|
     -> Result<(), ArcConsistencyFailure> {
        let slot_config = &config.slot_configs[slot_id];

        slot_states[slot_id].eliminations.insert(word_id);
        slot_states[slot_id].option_count -= 1;
        if let Some(blamed_cell_idx) = blamed_cell_idx {
            slot_states[slot_id].blame_counts[blamed_cell_idx] += 1;
        }

        // If this was the last option for the slot, we've failed to establish arc
        // consistency and need to bail out and return the relevant slot weights.
        if slot_states[slot_id].option_count == 0 {
            let initial_count = initial_option_counts[slot_id] as f32;

            return Err(ArcConsistencyFailure {
                weight_updates: slot_config
                    .crossings
                    .iter()
                    .enumerate()
                    .filter_map(|(cell_idx, crossing)| {
                        crossing.as_ref().map(|crossing| {
                            // We'll increment the weight of each constraint affecting this slot
                            // by the number of options it removed divided by the number of
                            // options we started with (IOW, the percentage of the slot's
                            // options that were removed by this constraint).
                            //
                            // You could argue that we should also track things like uniqueness
                            // constraints here, but this would add a lot of extra work to
                            // calculating slot weights since we'd have to check every slot in
                            // the grid pairwise every time, so it doesn't really seem worth it.
                            (
                                crossing.crossing_id,
                                (slot_states[slot_id].blame_counts[cell_idx] as f32)
                                    / initial_count,
                            )
                        })
                    })
                    .collect(),
            });
        }

        // If this was the *second*-to-last option for the slot, we'll want to propagate dupe rules,
        // etc., using that slot's now-locked-in value.
        if slot_states[slot_id].option_count == 1 {
            slot_states[slot_id].needs_singleton_propagation = true;
        }

        // Now we need to go through the letters of this word and decrement the glyph count for each
        // one. If any of them reach 0, and the crossing slot has a corresponding non-zero count, we
        // need to enqueue this cell to remove the no-longer-valid options from the crossing slot.
        for cell_idx in 0..slot_config.length {
            let glyph_id = config.word_list.words[slot_config.length][word_id].glyphs[cell_idx];

            let glyph_counts_for_cell =
                &mut slot_states[slot_id].get_glyph_counts(adapter)[cell_idx];

            glyph_counts_for_cell[glyph_id] -= 1;

            // If the reason we're removing this word is that it conflicted with this crossing slot,
            // we don't need to enqueue it because we already know the crossing doesn't have any
            // matching options.
            if blamed_cell_idx == Some(cell_idx) {
                continue;
            }

            // Otherwise, if this was the last word in the slot that contained this
            // glyph in this position, and there's a crossing entry that has at least one option
            // relying on the glyph, enqueue the cell so that we can propagate the impact further.
            if glyph_counts_for_cell[glyph_id] == 0 {
                let Some(crossing) = &slot_config.crossings[cell_idx] else {
                    continue;
                };

                if fixed_slots[crossing.other_slot_id] {
                    continue;
                }

                let crossing_glyph_count = slot_states[crossing.other_slot_id]
                    .get_glyph_counts(adapter)[crossing.other_slot_cell][glyph_id];

                if crossing_glyph_count > 0 {
                    if slot_states[slot_id].queued_cell_idxs.is_none() {
                        slot_states[slot_id].queued_cell_idxs =
                            Some(SmallVec::with_capacity(slot_config.length));
                    }
                    let queued_cell_idxs = slot_states[slot_id].queued_cell_idxs.as_mut().unwrap();

                    if !queued_cell_idxs.contains(&cell_idx) {
                        queued_cell_idxs.push(cell_idx);
                    }
                }
            }
        }

        Ok(())
    };

    // This propagation process has two phases that alternate until we're no longer removing any
    // values:
    //
    // * A regular AC-3 pass that propagates constraints between crossing words based on the letters
    //   available in their shared cells.
    //
    // * A singleton propagation pass that applies uniqueness rules (and potentially any other
    //   special constraints we want to add later) to slots that now only have a single option. This
    //   is a separate phase because these rules are difficult or impossible to fit into our AC-3
    //   structure without spoiling our ability to check option viability in constant time, and also
    //   because the vast majority of the benefit in terms of pruning will happen only in cases
    //   where a slot is limited to a single option.
    //
    // Once we've run both passes without enqueueing anything for either, we know we're done with
    // the overall process.
    //
    loop {
        // First, run the AC-3 algorithm, propagating eliminations until the queue is empty.
        loop {
            // Identify the queued slot with the lowest `dom/wdeg`, based on our live domain sizes.
            let slot_id = (0..config.slot_configs.len())
                .filter(|&slot_id| slot_states[slot_id].queued_cell_idxs.is_some())
                .min_by_key(|&slot_id| {
                    FloatOrd((slot_states[slot_id].option_count as f32) / slot_weights[slot_id])
                });

            // If there are no queued slots left, we're done with this AC pass.
            let Some(slot_id) = slot_id else {
                break;
            };

            // We want to examine the slot's cells in descending order of crossing weight.
            let mut cell_idxs =
                mem::replace(&mut slot_states[slot_id].queued_cell_idxs, None).unwrap();
            cell_idxs.sort_by_cached_key(|&cell_idx| {
                let crossing_id = config.slot_configs[slot_id].crossings[cell_idx]
                    .as_ref()
                    .expect("queued cell_idx must have a crossing")
                    .crossing_id;
                Reverse(FloatOrd(crossing_weights[crossing_id]))
            });

            // For each queued cell, go through the crossing slot's options and eliminate any that
            // are incompatible with this slot's possible values.
            for cell_idx in cell_idxs {
                let &Crossing {
                    other_slot_id,
                    other_slot_cell,
                    ..
                } = config.slot_configs[slot_id].crossings[cell_idx]
                    .as_ref()
                    .unwrap();

                let other_slot_config = &config.slot_configs[other_slot_id];
                let other_slot_options = &config.slot_options[other_slot_id];

                for &slot_option_word_id in other_slot_options.iter() {
                    // If this word has already been eliminated, we don't need to check it again.
                    if adapter.is_word_eliminated(other_slot_id, slot_option_word_id)
                        || slot_states[other_slot_id]
                            .eliminations
                            .contains(&slot_option_word_id)
                    {
                        continue;
                    }

                    let slot_option_word =
                        &config.word_list.words[other_slot_config.length][slot_option_word_id];
                    let slot_option_glyph = slot_option_word.glyphs[other_slot_cell];

                    let number_of_matching_options =
                        slot_states[slot_id].get_glyph_counts(adapter)[cell_idx][slot_option_glyph];

                    // If this word contains a glyph in the crossing cell that doesn't correspond to
                    // any options available in this cell, we need to eliminate it as an option.
                    if number_of_matching_options == 0 {
                        eliminate_word(
                            &mut slot_states,
                            other_slot_id,
                            slot_option_word_id,
                            Some(other_slot_cell),
                        )?;
                    }
                }
            }
        }

        // Now, if any slots need singleton propagation, we'll need to deal with that.
        let singleton_propagation_slot_ids: Vec<SlotId> = slot_states
            .iter_mut()
            .filter(|slot_state| slot_state.needs_singleton_propagation)
            .map(|slot_state| {
                slot_state.needs_singleton_propagation = false; // Reset flag on the way by
                slot_state.slot_id
            })
            .collect();

        for slot_id in singleton_propagation_slot_ids {
            let slot_config = &config.slot_configs[slot_id];
            let word_id = adapter
                .get_single_option(slot_id, &slot_states[slot_id].eliminations)
                .expect("slot with `needs_singleton_propagation` must have exactly one option");

            // Just eliminate this exact word from other slots, which is sometimes enough.
            let mut eliminate_simple_dupes = || -> Result<(), ArcConsistencyFailure> {
                for other_slot_id in 0..config.slot_configs.len() {
                    if other_slot_id != slot_id
                        && !fixed_slots[other_slot_id]
                        && config.slot_configs[other_slot_id].length == slot_config.length
                        && !adapter.is_word_eliminated(other_slot_id, word_id)
                        && !slot_states[other_slot_id].eliminations.contains(&word_id)
                        && config.slot_options[other_slot_id].contains(&word_id)
                    {
                        eliminate_word(&mut slot_states, other_slot_id, word_id, None)?;
                    }
                }
                Ok(())
            };

            if let Some(dupe_index) = &config.word_list.dupe_index {
                // For slots that are below the substring limit, simple dupes aren't covered by the index so we need to check for them directly.
                if slot_config.length < dupe_index.window_size() {
                    eliminate_simple_dupes()?;
                }

                // We also check the index regardless of the length since there may be additional dupe pairs containing short words.
                let dupes_by_length = dupe_index.get_dupes_by_length((slot_config.length, word_id));

                for other_slot_id in 0..config.slot_configs.len() {
                    if other_slot_id == slot_id || fixed_slots[other_slot_id] {
                        continue;
                    }

                    let later_slot_config = &config.slot_configs[other_slot_id];
                    let later_slot_options = &config.slot_options[other_slot_id];

                    let dupe_ids = dupes_by_length
                        .as_ref()
                        .and_then(|dupes_by_length| dupes_by_length.get(&later_slot_config.length));

                    if let Some(dupe_ids) = dupe_ids {
                        for &word_id in later_slot_options.iter() {
                            if !adapter.is_word_eliminated(other_slot_id, word_id)
                                && dupe_ids.contains(&word_id)
                                && !slot_states[other_slot_id].eliminations.contains(&word_id)
                            {
                                eliminate_word(&mut slot_states, other_slot_id, word_id, None)?;
                            }
                        }
                    }
                }
            } else {
                eliminate_simple_dupes()?;
            }

            // Any other special constraints could also be added here (e.g., two words not being
            // allowed to appear together). Any kind of constraint is OK as long as it's
            // symmetrical, since we assume that enforcing a constraint in one direction makes it
            // unnecessary to recheck in the other direction.
        }

        // If we no longer need either kind of propagation, we're done; otherwise, we return to the
        // top of the loop.
        if slot_states.iter().all(|slot_state| {
            slot_state.queued_cell_idxs.is_none() && !slot_state.needs_singleton_propagation
        }) {
            break;
        }
    }

    Ok(ArcConsistencySuccess {
        eliminations: slot_states
            .into_iter()
            .map(|slot_state| slot_state.eliminations)
            .collect(),
    })
}

/// Return a set of options to eliminate for each slot in the given grid config in order to
/// establish arc consistency.
#[allow(dead_code)]
pub fn establish_arc_consistency_for_static_grid(
    config: &GridConfig,
) -> Result<Vec<HashSet<WordId>>, ArcConsistencyFailure> {
    struct Adapter<'a> {
        config: &'a GridConfig<'a>,
    }

    impl<'a> ArcConsistencyAdapter for Adapter<'a> {
        fn is_word_eliminated(&self, _slot_id: SlotId, _word_id: WordId) -> bool {
            false
        }

        fn get_glyph_counts(&self, slot_id: SlotId) -> GlyphCountsByCell {
            build_glyph_counts_by_cell(
                self.config.word_list,
                self.config.slot_configs[slot_id].length,
                &self.config.slot_options[slot_id],
            )
        }

        fn get_single_option(
            &self,
            slot_id: SlotId,
            eliminations: &HashSet<WordId>,
        ) -> Option<WordId> {
            self.config.slot_options[slot_id]
                .iter()
                .find(|word_id| !eliminations.contains(word_id))
                .copied()
        }
    }

    let remaining_option_counts: Vec<usize> = (0..config.slot_configs.len())
        .map(|slot_id| config.slot_options[slot_id].len())
        .collect();

    let fixed_slots: Vec<bool> = (0..config.slot_configs.len())
        .map(|slot_id| {
            config.slot_configs[slot_id]
                .complete_fill(config.fill, config.width)
                .is_some()
        })
        .collect();

    // Since we don't know anything about which constraints are the most problematic, slot weight
    // is defined as "number of non-fixed crossing entries".
    let constraint_weights: Vec<f32> = (0..config.crossing_count).map(|_| 1.0).collect();
    let slot_weights: Vec<f32> = (0..config.slot_configs.len())
        .map(|slot_id| {
            config.slot_configs[slot_id]
                .crossings
                .iter()
                .filter(|crossing| {
                    crossing
                        .as_ref()
                        .map_or(false, |crossing| !fixed_slots[crossing.other_slot_id])
                })
                .count() as f32
        })
        .collect();

    let adapter = Adapter { config };

    establish_arc_consistency(
        config,
        &adapter,
        &remaining_option_counts,
        &constraint_weights,
        &slot_weights,
        &fixed_slots,
        None,
    )
    .map(|ArcConsistencySuccess { eliminations }| eliminations)
}

#[cfg(test)]
mod tests {
    use crate::arc_consistency::establish_arc_consistency_for_static_grid;
    use crate::grid_config::{generate_grid_config_from_template_string, OwnedGridConfig};
    use crate::word_list::tests::dictionary_path;
    use crate::word_list::WordList;
    use std::time::Instant;

    fn generate_config(template: &str) -> OwnedGridConfig {
        let template = template.trim();
        let width = template.lines().map(str::len).max().unwrap();
        let height = template.lines().count();
        let word_list =
            WordList::from_dict_file(&dictionary_path(), Some(width.max(height)), Some(5)).unwrap();

        generate_grid_config_from_template_string(word_list, template, 40.0)
    }

    #[test]
    fn test_establish_arc_consistency_for_static_grid() {
        // This grid is Ryan McCarty's "Chasm No. 1", with some words populated (including all the
        // words in the real puzzle that don't have a score of 40 or higher in STWL), as a
        // representative example of a very open grid.
        let mut grid_config = generate_config(
            "
            smashcake###.e.
            ......l..##..d.
            oreothins#...g.
            ......s.#....e.
            ###...a#b....l.
            ......#.l....o.
            .....#..o.#..r.
            ....#...o..#.d.
            #.......d...###
            ##......h....##
            ###soldierants#
            ...#....a.#....
            ....#...t#.....
            ........#......
            .......#....###
            ......#........
            .....#.........
            ....##.........
            ...###badassery
            ",
        );

        let start = Instant::now();

        let eliminations_by_slot =
            establish_arc_consistency_for_static_grid(&grid_config.to_config_ref())
                .expect("Failed to establish consistency");

        let checkpoint = start.elapsed();
        println!("Slot options eliminated in {:?}", start.elapsed());

        for (slot_id, slot_options) in grid_config.slot_options.iter_mut().enumerate() {
            slot_options.retain(|word_id| !eliminations_by_slot[slot_id].contains(word_id));
        }

        println!("Options pruned in {:?}", start.elapsed() - checkpoint);

        let opts = &grid_config.slot_options;
        assert_eq!(opts[0].len(), 1, "filled-in entry has one option");
        assert_eq!(
            opts[6].len(),
            5,
            "parallel entry has reduced number of options"
        );
        assert_eq!(
            opts[39].len(),
            2,
            "entry crossing seeds has very few options"
        );
    }
}
