use generic_array::typenum::{Unsigned, U10, U11, U4, U5, U6, U7, U8, U9};
use generic_array::{ArrayLength, GenericArray};
use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

use crate::{MAX_GLYPH_COUNT, MAX_SLOT_LENGTH};

lazy_static! {
    /// Completely arbitrary mapping from letter to point value.
    static ref LETTER_POINTS: HashMap<char, i32> = {
        let chars_and_scores: Vec<(&str, i32)> = vec![
            ("aeilnorstu", 1),
            ("dg", 2),
            ("bcmp", 3),
            ("fhvwy", 4),
            ("k", 5),
            ("jx", 8),
            ("qz", 10),
        ];
        HashMap::from_iter(
            chars_and_scores
                .iter()
                .flat_map(|(chars_str, score)| chars_str.chars().map(|char| (char, *score))),
        )
    };
}

/// An identifier for a given letter or symbol, based on its index in the WordList's `glyphs`
/// field.
pub type GlyphId = usize;

/// An identifier for a given word, based on its index in the WordList's `words` field (scoped to
/// the relevant length bucket).
pub type WordId = usize;

/// An identifier that fully specifies a word by including both its length and WordId.
pub type GlobalWordId = (usize, WordId);

/// A struct representing a word in the word list.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Word {
    /// The word as it would appear in a grid -- only lowercase letters or other valid glyphs.
    pub normalized_string: String,

    /// The word as it appears in the user's word list, with arbitrary formatting and punctuation.
    pub canonical_string: String,

    /// The glyph ids making up `normalized_string`.
    pub glyphs: SmallVec<[GlyphId; MAX_SLOT_LENGTH]>,

    /// The word's score, usually on a roughly 0.0 - 100.0 scale where 50.0 means average quality.
    pub score: f32,

    /// The sum of the scores of the word's letters.
    pub letter_score: f32,

    /// Is this word currently available for autofill? This will be false for non-words that are
    /// part of an input grid or for words that have been "removed" from the list dynamically.
    pub hidden: bool,
}

/// A struct used to track which words in the list share N-letter substrings, so that we can
/// efficiently enforce rules against choosing overlapping words. This is generic over the window
/// size, because we use fixed-length arrays to represent the substrings for performance reasons.
#[derive(Clone)]
struct DupeIndex<N: ArrayLength<GlyphId>> {
    /// The number of sequential characters that have to be shared to count as a dupe.
    window_size: usize,

    /// An array of groups of words that share a given substring.
    groups: Vec<Vec<GlobalWordId>>,

    /// For a given word, an array of group ids (indices of `groups`) that it belongs to.
    group_keys_by_word: HashMap<GlobalWordId, Vec<usize>>,

    /// For a given substring, the index in `groups` that represents words containing it.
    group_key_by_substring: HashMap<GenericArray<GlyphId, N>, usize>,
}

impl<N: ArrayLength<GlyphId>> Default for DupeIndex<N> {
    /// Build an empty DupeIndex.
    fn default() -> Self {
        DupeIndex {
            window_size: <N as Unsigned>::to_usize(),
            groups: vec![],
            group_keys_by_word: HashMap::new(),
            group_key_by_substring: HashMap::new(),
        }
    }
}

/// Interface representing a DupeIndex with any window size.
pub trait AnyDupeIndex {
    fn window_size(&self) -> usize;
    fn add_word(&mut self, word_id: WordId, word: &Word);
    fn get_dupes_by_length(&self, word_gid: GlobalWordId) -> HashMap<usize, HashSet<WordId>>;
}

impl<N: ArrayLength<GlyphId>> AnyDupeIndex for DupeIndex<N> {
    /// Accessor for the index's window size (the number of sequential characters that have to be
    /// shared to count as a dupe).
    fn window_size(&self) -> usize {
        self.window_size
    }

    /// Record a word in the index, adding it to the groups representing each of its
    /// `window_size`-length substrings.
    fn add_word(&mut self, word_id: WordId, word: &Word) {
        let word_gid = (word.glyphs.len(), word_id);
        let mut group_keys: Vec<usize> = vec![];

        for substring in word.glyphs.windows(self.window_size) {
            let substring = GenericArray::clone_from_slice(substring);
            let existing_group_key = self.group_key_by_substring.get(&substring);

            if let Some(&existing_group_key) = existing_group_key {
                self.groups[existing_group_key].push(word_gid);
                group_keys.push(existing_group_key);
            } else {
                let group_key = self.groups.len();
                self.groups.push(vec![word_gid]);
                self.group_key_by_substring.insert(substring, group_key);
                group_keys.push(group_key);
            }
        }

        self.group_keys_by_word.insert(word_gid, group_keys);
    }

    /// For a given word, get a map containing all words that duplicate it, indexed by their length.
    fn get_dupes_by_length(&self, word_gid: GlobalWordId) -> HashMap<usize, HashSet<WordId>> {
        let mut dupes_by_length: HashMap<usize, HashSet<WordId>> = HashMap::new();

        if let Some(group_ids) = self.group_keys_by_word.get(&word_gid) {
            for &group_id in group_ids {
                for &(length, word) in &self.groups[group_id] {
                    dupes_by_length
                        .entry(length)
                        .or_insert_with(HashSet::new)
                        .insert(word);
                }
            }
        }

        dupes_by_length
    }
}

/// A struct representing the currently-loaded word list(s). This contains information that is
/// static regardless of grid geometry or our progress through a fill (although we do configure a
/// `max_length` that depends on the size of the grid, since it helps performance to avoid
/// loading words that are too long to be usable). The word list can be modified without disrupting
/// the indices of any glyphs or words.
#[allow(dead_code)]
pub struct WordList {
    /// A list of all characters that occur in any (normalized) word. `GlyphId`s used everywhere
    /// else are indices into this list.
    pub glyphs: SmallVec<[char; MAX_GLYPH_COUNT]>,

    /// The inverse of `glyphs`: a map from a character to the `GlyphId` representing it.
    pub glyph_id_by_char: HashMap<char, GlyphId>,

    /// A list of all loaded words, bucketed by length. An index into `words` is the length of the
    /// words in the bucket, so `words[0]` is always an empty vec.
    pub words: Vec<Vec<Word>>,

    /// A map from a normalized string to the id of the Word representing it.
    pub word_id_by_string: HashMap<String, WordId>,

    /// A dupe index reflecting the max substring length provided when configuring the WordList.
    pub dupe_index: Option<Box<dyn AnyDupeIndex + Send + Sync>>,

    /// The maximum word length provided when configuring the WordList.
    pub max_length: usize,
}

/// A single word list entry, as provided when instantiating or updating WordList.
#[allow(dead_code)]
pub struct RawWordListEntry {
    pub normalized: String,
    pub canonical: String,
    pub score: i32,
}

impl WordList {
    /// Construct a new WordList containing the given entries (omitting any that are longer than
    /// `max_length`).
    #[allow(dead_code)]
    pub fn new(
        raw_word_list: &[RawWordListEntry],
        max_length: usize,
        max_shared_substring: Option<usize>,
    ) -> WordList {
        let mut instance = WordList {
            glyphs: smallvec![],
            glyph_id_by_char: HashMap::new(),
            words: (0..max_length + 1).map(|_| vec![]).collect(),
            word_id_by_string: HashMap::new(),
            dupe_index: WordList::instantiate_dupe_index(max_shared_substring),
            max_length,
        };

        instance.replace_list(raw_word_list, max_length);

        instance
    }

    /// Add the given word to the list. The word must not be part of the list yet.
    pub fn add_word(&mut self, raw_entry: &RawWordListEntry, hidden: bool) -> (WordId, &Word) {
        let word_length = raw_entry.normalized.len();
        let word_id = self.words[word_length].len();

        let glyphs: SmallVec<[GlyphId; MAX_SLOT_LENGTH]> = raw_entry
            .normalized
            .chars()
            .map(|c| self.glyph_id_for_char(c))
            .collect();

        self.words[word_length].push(Word {
            normalized_string: raw_entry.normalized.clone(),
            canonical_string: raw_entry.canonical.clone(),
            glyphs,
            score: raw_entry.score as f32,
            letter_score: raw_entry
                .normalized
                .chars()
                .map(|char| LETTER_POINTS.get(&char).cloned().unwrap_or(3))
                .sum::<i32>() as f32,
            hidden,
        });

        self.word_id_by_string
            .insert(raw_entry.normalized.clone(), word_id);
        if let Some(dupe_index) = &mut self.dupe_index {
            dupe_index.add_word(word_id, &self.words[word_length][word_id]);
        }

        (word_id, &self.words[word_length][word_id])
    }

    /// Add all of the given words to the list, and hide any non-hidden words that aren't included
    /// in the new list. We don't fully remove them because we want to keep all of the ids stable
    /// and they may still be referenced elsewhere.
    pub fn replace_list(
        &mut self,
        raw_word_list: &[RawWordListEntry],
        max_length: usize,
    ) -> HashSet<GlobalWordId> {
        // Start with the assumption that we're removing everything.
        let mut removed_words_map = self.word_id_by_string.clone();

        // If we're expanding our previous max length, make sure the `words` vec has enough entries.
        while self.words.len() < max_length + 1 {
            self.words.push(vec![]);
        }
        self.max_length = max_length;

        // Now go through our new words and add them.
        for raw_entry in raw_word_list.iter() {
            let word_length = raw_entry.normalized.len();
            if word_length > max_length {
                continue;
            }

            let existing_word_id = self.word_id_by_string.get(&raw_entry.normalized);

            if let Some(&existing_word_id) = existing_word_id {
                let word = &mut self.words[word_length][existing_word_id];
                word.score = raw_entry.score as f32;
                word.hidden = false;
                word.canonical_string = raw_entry.canonical.clone();
                removed_words_map.remove(&raw_entry.normalized);
            } else {
                self.add_word(raw_entry, false);
            }
        }

        // Finally, hide any words that were in our existing list but aren't in the new one.
        let removed_words_set: HashSet<GlobalWordId> = removed_words_map
            .iter()
            .map(|(word_str, &word_id)| {
                let word_length = word_str.len();
                self.words[word_length][word_id].hidden = true;
                (word_length, word_id)
            })
            .collect();

        removed_words_set
    }

    /// What's the unique glyph id for the given char? We do this lazily, instead of just mapping
    /// every letter up front, because word list entries may also contain numbers or non-English
    /// letters.
    pub fn glyph_id_for_char(&mut self, ch: char) -> GlyphId {
        self.glyph_id_by_char.get(&ch).cloned().unwrap_or_else(|| {
            self.glyphs.push(ch);
            let id = self.glyphs.len() - 1;
            self.glyph_id_by_char.insert(ch, id);
            id
        })
    }

    /// Update the `max_shared_substring` config by regenerating the dupe index.
    #[allow(dead_code)]
    pub fn update_max_shared_substring(&mut self, max_shared_substring: Option<usize>) {
        let mut new_dupe_index = WordList::instantiate_dupe_index(max_shared_substring);

        if let Some(new_dupe_index) = &mut new_dupe_index {
            for bucket in self.words.iter() {
                for (word_id, word) in bucket.iter().enumerate() {
                    new_dupe_index.add_word(word_id, word);
                }
            }
        }

        self.dupe_index = new_dupe_index;
    }

    /// Generate a DupeIndex with the appropriate window size for the given `max_shared_substring`
    /// setting. This is ugly because we want to be able to use raw arrays in the implementation of
    /// DupeIndex, so their lengths need to be known at compile time.
    fn instantiate_dupe_index(
        max_shared_substring: Option<usize>,
    ) -> Option<Box<dyn AnyDupeIndex + Send + Sync>> {
        // The type param is one higher than `max_shared_substring` because it's smallest forbidden
        // overlap, not max shared substring.
        match max_shared_substring {
            Some(0) => None,
            Some(1) => None,
            Some(2) => None,
            Some(3) => Some(Box::new(DupeIndex::<U4>::default())),
            Some(4) => Some(Box::new(DupeIndex::<U5>::default())),
            Some(5) => Some(Box::new(DupeIndex::<U6>::default())),
            Some(6) => Some(Box::new(DupeIndex::<U7>::default())),
            Some(7) => Some(Box::new(DupeIndex::<U8>::default())),
            Some(8) => Some(Box::new(DupeIndex::<U9>::default())),
            Some(9) => Some(Box::new(DupeIndex::<U10>::default())),
            Some(10) => Some(Box::new(DupeIndex::<U11>::default())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::word_list::{RawWordListEntry, WordList};
    use std::{fs, path};

    fn load_dictionary() -> Vec<RawWordListEntry> {
        let mut path = path::PathBuf::from(file!());
        path.pop();
        path.pop();
        path.push("resources");
        path.push("spreadthewordlist.dict");

        fs::read_to_string(&path)
            .expect("Something went wrong reading the file")
            .lines()
            .map(|line| {
                let line_parts: Vec<_> = line.split(';').collect();
                let canonical = line_parts[0].to_string();
                let normalized: String = canonical
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                let score: i32 = line_parts[1]
                    .parse()
                    .expect("Dict included non-numeric score");

                RawWordListEntry {
                    canonical,
                    normalized,
                    score,
                }
            })
            .collect()
    }

    #[test]
    fn test_loads_words_up_to_max_length() {
        let word_list = WordList::new(&load_dictionary(), 5, None);

        assert_eq!(word_list.max_length, 5);
        assert_eq!(word_list.words.len(), 6);

        let &word_id = word_list
            .word_id_by_string
            .get("skate")
            .expect("word list should include 'skate'");

        let word = &word_list.words[5][word_id];
        assert_eq!(word.normalized_string, "skate");
        assert_eq!(word.canonical_string, "skate");
        assert_eq!(word.score, 50.0);
        assert_eq!(word.hidden, false);

        assert!(matches!(word_list.word_id_by_string.get("skates"), None));
    }
}
