use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::Path;
use std::{fmt, fs};
use unicode_normalization::UnicodeNormalization;

use crate::dupe_index::{AnyDupeIndex, BoxedDupeIndex, DupeIndex};
use crate::types::{GlobalWordId, GlyphId, WordId};
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
        chars_and_scores
            .iter()
            .flat_map(|(chars_str, score)| chars_str.chars().map(|char| (char, *score)))
            .collect()
    };
}

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

/// Given a canonical word string from a dictionary file, turn it into the normalized form we'll
/// use in the actual fill engine.
#[must_use]
pub fn normalize_word(canonical: &str) -> String {
    canonical
        .to_lowercase()
        .nfc() // Normalize Unicode combining forms
        .filter(|c| !c.is_whitespace())
        .collect()
}

type OnUpdateCallback =
    Box<dyn FnMut(&mut WordList, &[GlobalWordId], &[GlobalWordId]) + Send + Sync>;

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
    pub dupe_index: BoxedDupeIndex,

    /// The maximum word length provided when configuring the WordList, if any.
    pub max_length: Option<usize>,

    /// Callback run after adding or removing words.
    pub on_update: Option<OnUpdateCallback>,
}

/// A single word list entry, as provided when instantiating or updating `WordList`.
#[allow(dead_code)]
pub struct RawWordListEntry {
    pub length: usize,
    pub normalized: String,
    pub canonical: String,
    pub score: i32,
}

impl RawWordListEntry {
    #[must_use]
    pub fn new(canonical: String, score: i32) -> RawWordListEntry {
        let normalized = normalize_word(&canonical);
        RawWordListEntry {
            length: normalized.chars().count(),
            normalized,
            canonical,
            score,
        }
    }
}

#[derive(Debug)]
pub enum WordListError {
    InvalidPath(String),
    InvalidLine(String),
    InvalidWord(String),
    InvalidScore(String),
}

impl fmt::Display for WordListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let string = match self {
            WordListError::InvalidPath(path) => format!("Can't read path: {path}"),
            WordListError::InvalidLine(line) => {
                format!("Word list contains invalid line: '{line}'")
            }
            WordListError::InvalidWord(word) => {
                format!("Word list contains invalid word: '{word}'")
            }
            WordListError::InvalidScore(score) => {
                format!("Word list contains invalid score: '{score}'")
            }
        };
        write!(f, "{string}")
    }
}

impl WordList {
    /// Instantiate a `WordList` based on the given `.dict` file. Convenience wrapper
    /// around `load_dict_file` and `new`.
    pub fn from_dict_file<P: AsRef<Path>>(
        path: P,
        max_length: Option<usize>,
        max_shared_substring: Option<usize>,
    ) -> Result<WordList, WordListError> {
        Ok(WordList::new(
            &WordList::load_dict_file(&path)?,
            max_length,
            max_shared_substring,
        ))
    }

    /// Parse the given `.dict` file contents into `RawWordListEntry` structs, if possible.
    pub fn parse_dict_file(file_contents: &str) -> Result<Vec<RawWordListEntry>, WordListError> {
        file_contents
            .lines()
            .map(|line| {
                let line_parts: Vec<_> = line.trim().split(';').collect();
                if line_parts.len() != 2 {
                    return Err(WordListError::InvalidLine(line.into()));
                }

                let canonical = line_parts[0].trim().to_string();
                let normalized: String = normalize_word(&canonical);
                if normalized.is_empty() {
                    return Err(WordListError::InvalidWord(line_parts[0].into()));
                }

                let Ok(score) = line_parts[1].trim().parse::<i32>() else {
                    return Err(WordListError::InvalidScore(line_parts[1].into()));
                };

                Ok(RawWordListEntry {
                    length: normalized.chars().count(),
                    normalized,
                    canonical,
                    score,
                })
            })
            .collect()
    }

    /// Load the contents of the given `.dict` file into `RawWordListEntry` structs, if possible.
    pub fn load_dict_file<P: AsRef<Path>>(path: P) -> Result<Vec<RawWordListEntry>, WordListError> {
        let Ok(file_contents) = fs::read_to_string(&path) else {
            return Err(WordListError::InvalidPath(format!("{:?}", path.as_ref())));
        };

        WordList::parse_dict_file(&file_contents)
    }

    /// Construct a new `WordList` containing the given entries (omitting any that are longer than
    /// `max_length`).
    #[allow(dead_code)]
    #[must_use]
    pub fn new(
        raw_word_list: &[RawWordListEntry],
        max_length: Option<usize>,
        max_shared_substring: Option<usize>,
    ) -> WordList {
        let mut instance = WordList {
            glyphs: smallvec![],
            glyph_id_by_char: HashMap::new(),
            words: vec![vec![]],
            word_id_by_string: HashMap::new(),
            dupe_index: WordList::instantiate_dupe_index(max_shared_substring),
            max_length,
            on_update: None,
        };

        instance.replace_list(raw_word_list, max_length, false);

        instance
    }

    /// If the given normalized word is already in the list, return its id; if not, add it as a
    /// hidden entry and return the id of that.
    pub fn get_word_id_or_add_hidden(&mut self, normalized_word: &str) -> GlobalWordId {
        self.word_id_by_string
            .get(normalized_word)
            .copied()
            .map_or_else(
                || {
                    self.add_word(
                        &RawWordListEntry {
                            length: normalized_word.chars().count(),
                            normalized: normalized_word.to_string(),
                            canonical: normalized_word.to_string(),
                            score: 0,
                        },
                        true,
                    )
                },
                |word_id| (normalized_word.chars().count(), word_id),
            )
    }

    /// Add the given word to the list and trigger the update callback. The word must not be part of the list yet.
    pub fn add_word(&mut self, raw_entry: &RawWordListEntry, hidden: bool) -> GlobalWordId {
        let global_word_id = self.add_word_silent(raw_entry, hidden);

        if let Some(mut on_update) = self.on_update.take() {
            on_update(self, &[global_word_id], &[]);
            self.on_update = Some(on_update);
        }

        global_word_id
    }

    /// Add the given word to the list without triggering the update callback. The word must not be part of the list yet.
    fn add_word_silent(&mut self, raw_entry: &RawWordListEntry, hidden: bool) -> GlobalWordId {
        let glyphs: SmallVec<[GlyphId; MAX_SLOT_LENGTH]> = raw_entry
            .normalized
            .chars()
            .map(|c| self.glyph_id_for_char(c))
            .collect();

        let word_length = glyphs.len();

        // We can add words above `max_length`, but we need to make sure there are enough buckets.
        while self.words.len() < word_length + 1 {
            self.words.push(vec![]);
        }

        let word_id = self.words[word_length].len();

        self.words[word_length].push(Word {
            normalized_string: raw_entry.normalized.clone(),
            canonical_string: raw_entry.canonical.clone(),
            glyphs,
            score: raw_entry.score as f32,
            letter_score: raw_entry
                .normalized
                .chars()
                .map(|char| LETTER_POINTS.get(&char).copied().unwrap_or(3))
                .sum::<i32>() as f32,
            hidden,
        });

        self.word_id_by_string
            .insert(raw_entry.normalized.clone(), word_id);

        self.dupe_index
            .add_word(word_id, &self.words[word_length][word_id]);

        (word_length, word_id)
    }

    /// Add all of the given words to the list, and hide any non-hidden words that aren't included
    /// in the new list. We don't fully remove them because we want to keep all of the ids stable
    /// and they may still be referenced elsewhere.
    ///
    /// We return a boolean indicating whether we added any words, along with a set of removed
    /// words. If `silent` is false and an `on_update` callback is present, we track both added and
    /// removed words and pass them into the callback.
    pub fn replace_list(
        &mut self,
        raw_word_list: &[RawWordListEntry],
        max_length: Option<usize>,
        silent: bool,
    ) -> (bool, HashSet<GlobalWordId>) {
        self.max_length = max_length;

        // Start with the assumption that we're removing everything.
        let mut removed_words_set: HashSet<GlobalWordId> = self
            .words
            .iter()
            .enumerate()
            .flat_map(|(length, word_ids)| {
                (0..word_ids.len()).map(move |word_id| (length, word_id))
            })
            .collect();
        let mut added_words: Vec<GlobalWordId> = vec![];
        let mut added_any_words = false;
        let silent = silent || self.on_update.is_none();

        // If we were given a max length, make sure we have that many buckets.
        if let Some(max_length) = max_length {
            while self.words.len() < max_length + 1 {
                self.words.push(vec![]);
            }
        }

        // Now go through our new words and add them.
        for raw_entry in raw_word_list {
            let word_length = raw_entry.length;
            if let Some(max_length) = max_length {
                if word_length > max_length {
                    continue;
                }
            }

            let existing_word_id = self.word_id_by_string.get(&raw_entry.normalized);

            if let Some(&existing_word_id) = existing_word_id {
                let word = &mut self.words[word_length][existing_word_id];
                word.score = raw_entry.score as f32;
                word.hidden = false;
                word.canonical_string = raw_entry.canonical.clone();
                removed_words_set.remove(&(word_length, existing_word_id));
            } else if !silent {
                added_any_words = true;
                let added_word_id = self.add_word_silent(raw_entry, false);
                added_words.push(added_word_id);
            } else {
                added_any_words = true;
                self.add_word_silent(raw_entry, false);
            }
        }

        // Finally, hide any words that were in our existing list but aren't in the new one.
        for &(length, word_id) in &removed_words_set {
            self.words[length][word_id].hidden = true;
        }

        if let Some(mut on_update) = self.on_update.take() {
            on_update(
                self,
                &added_words,
                &removed_words_set.iter().copied().collect::<Vec<_>>(),
            );
            self.on_update = Some(on_update);
        }

        (added_any_words, removed_words_set)
    }

    /// What's the unique glyph id for the given char? We do this lazily, instead of just mapping
    /// every letter up front, because word list entries may also contain numbers or non-English
    /// letters.
    pub fn glyph_id_for_char(&mut self, ch: char) -> GlyphId {
        self.glyph_id_by_char.get(&ch).copied().unwrap_or_else(|| {
            self.glyphs.push(ch);
            let id = self.glyphs.len() - 1;
            self.glyph_id_by_char.insert(ch, id);
            id
        })
    }

    /// Update the `max_shared_substring` config by regenerating the dupe index.
    pub fn update_max_shared_substring(&mut self, max_shared_substring: Option<usize>) {
        let extra_dupes = self.dupe_index.take_extra_dupes();
        let mut new_dupe_index = WordList::instantiate_dupe_index(max_shared_substring);
        self.populate_dupe_index(new_dupe_index.as_mut());
        new_dupe_index.put_extra_dupes(extra_dupes);
        self.dupe_index = new_dupe_index;
    }

    /// Generate a `DupeIndex` with the appropriate window size for the given `max_shared_substring`
    /// setting. This is ugly because we want to be able to use raw arrays in the implementation of
    /// `DupeIndex`, so their lengths need to be known at compile time.
    #[must_use]
    pub fn instantiate_dupe_index(max_shared_substring: Option<usize>) -> BoxedDupeIndex {
        // The type param is one higher than `max_shared_substring` because it's smallest forbidden
        // overlap, not max shared substring.
        match max_shared_substring {
            Some(3) => Box::<DupeIndex<4>>::default(),
            Some(4) => Box::<DupeIndex<5>>::default(),
            Some(5) => Box::<DupeIndex<6>>::default(),
            Some(6) => Box::<DupeIndex<7>>::default(),
            Some(7) => Box::<DupeIndex<8>>::default(),
            Some(8) => Box::<DupeIndex<9>>::default(),
            Some(9) => Box::<DupeIndex<10>>::default(),
            Some(10) => Box::<DupeIndex<11>>::default(),
            _ => Box::<DupeIndex<0>>::default(),
        }
    }

    pub fn populate_dupe_index(&self, index: &mut dyn AnyDupeIndex) {
        if index.window_size() == 0 {
            return;
        }
        for bucket in &self.words {
            for (word_id, word) in bucket.iter().enumerate() {
                index.add_word(word_id, word);
            }
        }
    }

    pub fn add_words_to_dupe_index(&self, index: &mut dyn AnyDupeIndex, words: &[GlobalWordId]) {
        if index.window_size() == 0 {
            return;
        }
        for &(length, word_id) in words {
            index.add_word(word_id, &self.words[length][word_id]);
        }
    }
}

impl Debug for WordList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WordList")
            .field("glyphs", &self.glyphs)
            .field(
                "words",
                &self.words.iter().map(Vec::len).collect::<Vec<_>>(),
            )
            .field("max_length", &self.max_length)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub mod tests {
    use crate::dupe_index::{AnyDupeIndex, DupeIndex};
    use crate::types::GlobalWordId;
    use crate::word_list::{RawWordListEntry, WordList};
    use std::path;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[must_use]
    pub fn dictionary_path() -> PathBuf {
        let mut path = path::PathBuf::from(file!());
        path.pop();
        path.pop();
        path.push("resources");
        path.push("spreadthewordlist.dict");
        path
    }

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    #[allow(clippy::float_cmp)]
    fn test_loads_words_up_to_max_length() {
        let word_list = WordList::from_dict_file(dictionary_path(), Some(5), None).unwrap();

        assert_eq!(word_list.max_length, Some(5));
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

    #[test]
    fn test_unusual_characters() {
        let word_list = WordList::new(
            &[
                // Non-English character expressed as one two-byte `char`
                RawWordListEntry::new("monsutâ".into(), 50),
                // Non-English character expressed as two chars w/ combining form
                RawWordListEntry::new("hélen".into(), 50),
            ],
            None,
            None,
        );

        assert_eq!(
            word_list
                .words
                .iter()
                .map(|bucket| bucket.len())
                .collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 1, 0, 1]
        );
    }

    #[test]
    fn test_soft_dupe_index() {
        let mut word_list = WordList::new(&[], Some(6), Some(5));
        let mut soft_dupe_index = DupeIndex::<4>::default();

        // This doesn't do anything except make sure it's OK to call this method unboxed
        word_list.populate_dupe_index(&mut soft_dupe_index);

        let soft_dupe_index = Arc::new(Mutex::new(soft_dupe_index));

        word_list.on_update = {
            let soft_dupe_index = soft_dupe_index.clone();

            Some(Box::new(
                move |word_list, added_word_ids, _removed_word_ids| {
                    let mut soft_dupe_index = soft_dupe_index.lock().unwrap();

                    for &(word_length, word_id) in added_word_ids {
                        soft_dupe_index.add_word(word_id, &word_list.words[word_length][word_id]);
                    }
                },
            ))
        };

        let golf_id = word_list.add_word(&RawWordListEntry::new("golf".into(), 0), false);

        let golfy_id = word_list.add_word(&RawWordListEntry::new("golfy".into(), 0), false);

        let golves_id = word_list.add_word(&RawWordListEntry::new("golves".into(), 0), false);

        let is_dupe = |index: &dyn AnyDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            index
                .get_dupes_by_length(id_1)
                .get(&id_2.0)
                .cloned()
                .map_or(false, |dupes| dupes.contains(&id_2.1))
        };

        let assert_dupe = |index: &dyn AnyDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            assert!(is_dupe(index, id_1, id_2), "is_dupe({id_1:?}, {id_2:?})");
            assert!(is_dupe(index, id_2, id_1), "is_dupe({id_2:?}, {id_1:?})");
        };

        let assert_not_dupe = |index: &dyn AnyDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            assert!(!is_dupe(index, id_1, id_2), "!is_dupe({id_1:?}, {id_2:?})");
            assert!(!is_dupe(index, id_2, id_1), "!is_dupe({id_2:?}, {id_1:?})");
        };

        let main_index = &word_list.dupe_index;
        let mut soft_dupe_index = soft_dupe_index.lock().unwrap();

        // Regular index doesn't see any of these as dupes
        assert_not_dupe(main_index.as_ref(), golf_id, golfy_id);
        assert_not_dupe(main_index.as_ref(), golf_id, golves_id);
        assert_not_dupe(main_index.as_ref(), golfy_id, golves_id);

        // Secondary index sees golf/golfy as dupes but not golf/golves
        assert_dupe(&*soft_dupe_index, golf_id, golfy_id);
        assert_not_dupe(&*soft_dupe_index, golf_id, golves_id);
        assert_not_dupe(&*soft_dupe_index, golfy_id, golves_id);

        // Add custom pair
        soft_dupe_index.add_dupe_pair(golf_id, golves_id);

        // After customization, golf/golves is a dupe
        assert_dupe(&*soft_dupe_index, golf_id, golfy_id);
        assert_dupe(&*soft_dupe_index, golf_id, golves_id);
        assert_not_dupe(&*soft_dupe_index, golfy_id, golves_id);

        // Remove custom pair and check again
        soft_dupe_index.remove_dupe_pair(golf_id, golves_id);
        assert_not_dupe(&*soft_dupe_index, golf_id, golves_id);
    }
}
