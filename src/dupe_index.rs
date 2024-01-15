use crate::types::{GlobalWordId, GlyphId, WordId};
use crate::word_list::Word;
use std::collections::{HashMap, HashSet};
use std::{mem, vec};

/// A struct used to track which words in the list share N-letter substrings, so that we can
/// efficiently enforce rules against choosing overlapping words. This is generic over the window
/// size, because we use fixed-length arrays to represent the substrings for performance reasons.
#[derive(Debug, Clone)]
pub struct DupeIndex<const WINDOW_SIZE: usize> {
    /// An array of groups of words that share a given substring.
    pub groups: Vec<Vec<GlobalWordId>>,

    /// A map of extra pairwise dupe relationships that aren't based on substrings.
    pub extra_dupes_by_word: HashMap<GlobalWordId, Vec<GlobalWordId>>,

    /// For a given word, an array of group ids (indices of `groups`) that it belongs to.
    pub group_keys_by_word: HashMap<GlobalWordId, Vec<usize>>,

    /// For a given substring, the index in `groups` that represents words containing it.
    pub group_key_by_substring: HashMap<[GlyphId; WINDOW_SIZE], usize>,
}

impl<const WINDOW_SIZE: usize> Default for DupeIndex<WINDOW_SIZE> {
    /// Build an empty `DupeIndex`.
    fn default() -> Self {
        DupeIndex {
            groups: vec![],
            extra_dupes_by_word: HashMap::new(),
            group_keys_by_word: HashMap::new(),
            group_key_by_substring: HashMap::new(),
        }
    }
}

/// Interface representing a `DupeIndex` with any window size.
pub trait AnyDupeIndex {
    fn window_size(&self) -> usize;
    fn add_word(&mut self, word_id: WordId, word: &Word);
    fn add_dupe_pair(&mut self, global_word_id_1: GlobalWordId, global_word_id_2: GlobalWordId);
    fn remove_dupe_pair(&mut self, global_word_id_1: GlobalWordId, global_word_id_2: GlobalWordId);
    fn get_dupes_by_length(&self, global_word_id: GlobalWordId) -> HashMap<usize, HashSet<WordId>>;

    // Allow moving extra dupe pairs in and out to facilitate replacing the word list.
    fn take_extra_dupes(&mut self) -> HashMap<GlobalWordId, Vec<GlobalWordId>>;
    fn put_extra_dupes(&mut self, extra_dupes: HashMap<GlobalWordId, Vec<GlobalWordId>>);
}

impl<const WINDOW_SIZE: usize> AnyDupeIndex for DupeIndex<WINDOW_SIZE> {
    /// Accessor for the index's window size (the number of sequential characters that have to be
    /// shared to count as a dupe).
    fn window_size(&self) -> usize {
        WINDOW_SIZE
    }

    /// Record a word in the index, adding it to the groups representing each of its
    /// `WINDOW_SIZE`-length substrings.
    fn add_word(&mut self, word_id: WordId, word: &Word) {
        // If `WINDOW_SIZE` is zero, this index only tracks extra dupes, not window-based dupes.
        if WINDOW_SIZE == 0 {
            return;
        }

        let global_word_id = (word.glyphs.len(), word_id);
        let mut group_keys: Vec<usize> = vec![];

        for substring_slice in word.glyphs.windows(WINDOW_SIZE) {
            let substring: [GlyphId; WINDOW_SIZE] = substring_slice.try_into().unwrap();
            let existing_group_key = self.group_key_by_substring.get(&substring);

            if let Some(&existing_group_key) = existing_group_key {
                self.groups[existing_group_key].push(global_word_id);
                group_keys.push(existing_group_key);
            } else {
                let group_key = self.groups.len();
                self.groups.push(vec![global_word_id]);
                self.group_key_by_substring.insert(substring, group_key);
                group_keys.push(group_key);
            }
        }

        self.group_keys_by_word.insert(global_word_id, group_keys);
    }

    /// Record that two arbitrary words should be considered duplicates of each other.
    fn add_dupe_pair(&mut self, global_word_id_1: GlobalWordId, global_word_id_2: GlobalWordId) {
        for (from_id, to_id) in [
            (global_word_id_1, global_word_id_2),
            (global_word_id_2, global_word_id_1),
        ] {
            if let Some(existing_group) = self.extra_dupes_by_word.get_mut(&from_id) {
                if !existing_group.contains(&to_id) {
                    existing_group.push(to_id);
                }
            } else {
                self.extra_dupes_by_word.insert(from_id, vec![to_id]);
            }
        }
    }

    /// Remove a word pair from the extra dupes index (note that they will still be considered dupes if they have a long enough overlapping substring, etc.).
    fn remove_dupe_pair(&mut self, global_word_id_1: GlobalWordId, global_word_id_2: GlobalWordId) {
        for (from_id, to_id) in [
            (global_word_id_1, global_word_id_2),
            (global_word_id_2, global_word_id_1),
        ] {
            if let Some(existing_group) = self.extra_dupes_by_word.get_mut(&from_id) {
                existing_group.retain(|id| *id != to_id);
            }
        }
    }

    /// For a given word, get a map containing all words that duplicate it, indexed by their length.
    fn get_dupes_by_length(&self, global_word_id: GlobalWordId) -> HashMap<usize, HashSet<WordId>> {
        let mut dupes_by_length: HashMap<usize, HashSet<WordId>> = HashMap::new();

        // All words duplicate themselves, regardless of length.
        dupes_by_length
            .entry(global_word_id.0)
            .or_insert_with(HashSet::new)
            .insert(global_word_id.1);

        let group_ids = self.group_keys_by_word.get(&global_word_id);
        let extra_dupes = self.extra_dupes_by_word.get(&global_word_id);

        if let Some(group_ids) = group_ids {
            for &group_id in group_ids {
                for &(length, word) in &self.groups[group_id] {
                    dupes_by_length
                        .entry(length)
                        .or_insert_with(HashSet::new)
                        .insert(word);
                }
            }
        }

        if let Some(extra_dupes) = extra_dupes {
            for &(length, word) in extra_dupes.iter() {
                dupes_by_length
                    .entry(length)
                    .or_insert_with(HashSet::new)
                    .insert(word);
            }
        }

        dupes_by_length
    }

    fn take_extra_dupes(&mut self) -> HashMap<GlobalWordId, Vec<GlobalWordId>> {
        mem::take(&mut self.extra_dupes_by_word)
    }

    fn put_extra_dupes(&mut self, extra_dupes: HashMap<GlobalWordId, Vec<GlobalWordId>>) {
        self.extra_dupes_by_word = extra_dupes;
    }
}

pub type BoxedDupeIndex = Box<dyn AnyDupeIndex + Send + Sync>;
