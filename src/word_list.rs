use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::Path;
use std::{fmt, fs};

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

/// An identifier for a given letter or symbol, based on its index in the `WordList`'s `glyphs`
/// field.
pub type GlyphId = usize;

/// An identifier for a given word, based on its index in the `WordList`'s `words` field (scoped to
/// the relevant length bucket).
pub type WordId = usize;

/// An identifier that fully specifies a word by including both its length and `WordId`.
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
    fn get_dupes_by_length(
        &self,
        global_word_id: GlobalWordId,
    ) -> Option<HashMap<usize, HashSet<WordId>>>;
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
    fn get_dupes_by_length(
        &self,
        global_word_id: GlobalWordId,
    ) -> Option<HashMap<usize, HashSet<WordId>>> {
        let group_ids = self.group_keys_by_word.get(&global_word_id);
        let extra_dupes = self.extra_dupes_by_word.get(&global_word_id);
        if group_ids.is_none() && extra_dupes.is_none() {
            return None;
        }

        let mut dupes_by_length: HashMap<usize, HashSet<WordId>> = HashMap::new();

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

        Some(dupes_by_length)
    }
}

type BoxedDupeIndex = Box<dyn AnyDupeIndex + Send + Sync>;

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
    pub dupe_index: Option<BoxedDupeIndex>,

    /// The maximum word length provided when configuring the WordList.
    pub max_length: usize,
}

/// A single word list entry, as provided when instantiating or updating `WordList`.
#[allow(dead_code)]
pub struct RawWordListEntry {
    pub normalized: String,
    pub canonical: String,
    pub score: i32,
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
                let normalized: String = canonical
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if normalized.is_empty() {
                    return Err(WordListError::InvalidWord(line_parts[0].into()));
                }

                let Ok(score) = line_parts[1].trim().parse::<i32>() else {
                    return Err(WordListError::InvalidScore(line_parts[1].into()));
                };

                Ok(RawWordListEntry {
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
        let max_length = max_length.unwrap_or_else(|| {
            raw_word_list
                .iter()
                .map(|entry| entry.normalized.len())
                .max()
                .unwrap_or(0)
        });

        let mut instance = WordList {
            glyphs: smallvec![],
            glyph_id_by_char: HashMap::new(),
            words: (0..=max_length).map(|_| vec![]).collect(),
            word_id_by_string: HashMap::new(),
            dupe_index: WordList::instantiate_dupe_index(max_shared_substring),
            max_length,
        };

        instance.replace_list(raw_word_list, max_length, false);

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
                .map(|char| LETTER_POINTS.get(&char).copied().unwrap_or(3))
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
    ///
    /// We return a tuple of two sets, one of added words and one of removed words. As an
    /// optimization, we don't populate the added set unless the caller asks us to (since it's
    /// pointless when we're initializing the list for the first time and everything is new).
    pub fn replace_list(
        &mut self,
        raw_word_list: &[RawWordListEntry],
        max_length: usize,
        track_added_set: bool,
    ) -> (HashSet<GlobalWordId>, HashSet<GlobalWordId>) {
        // Start with the assumption that we're removing everything.
        let mut removed_words_map = self.word_id_by_string.clone();
        let mut added_words_set = HashSet::new();

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
            } else if track_added_set {
                let (added_word_id, _) = self.add_word(raw_entry, false);
                added_words_set.insert((word_length, added_word_id));
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

        (added_words_set, removed_words_set)
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
        let mut new_dupe_index = WordList::instantiate_dupe_index(max_shared_substring);

        if let Some(new_dupe_index) = &mut new_dupe_index {
            self.populate_dupe_index(new_dupe_index);
        }

        self.dupe_index = new_dupe_index;
    }

    /// Generate a `DupeIndex` with the appropriate window size for the given `max_shared_substring`
    /// setting. This is ugly because we want to be able to use raw arrays in the implementation of
    /// `DupeIndex`, so their lengths need to be known at compile time.
    #[must_use]
    pub fn instantiate_dupe_index(max_shared_substring: Option<usize>) -> Option<BoxedDupeIndex> {
        // The type param is one higher than `max_shared_substring` because it's smallest forbidden
        // overlap, not max shared substring.
        match max_shared_substring {
            Some(3) => Some(Box::<DupeIndex<4>>::default()),
            Some(4) => Some(Box::<DupeIndex<5>>::default()),
            Some(5) => Some(Box::<DupeIndex<6>>::default()),
            Some(6) => Some(Box::<DupeIndex<7>>::default()),
            Some(7) => Some(Box::<DupeIndex<8>>::default()),
            Some(8) => Some(Box::<DupeIndex<9>>::default()),
            Some(9) => Some(Box::<DupeIndex<10>>::default()),
            Some(10) => Some(Box::<DupeIndex<11>>::default()),
            _ => None,
        }
    }

    fn populate_dupe_index(&self, index: &mut BoxedDupeIndex) {
        for bucket in &self.words {
            for (word_id, word) in bucket.iter().enumerate() {
                index.add_word(word_id, word);
            }
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
            .finish()
    }
}

#[cfg(test)]
pub mod tests {
    use crate::word_list::{BoxedDupeIndex, DupeIndex, GlobalWordId, RawWordListEntry, WordList};
    use std::path;
    use std::path::PathBuf;

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

    #[test]
    fn test_soft_dupe_index() {
        let mut word_list = WordList::new(&[], Some(6), Some(5));
        let mut soft_dupe_index: BoxedDupeIndex = Box::new(DupeIndex::<4>::default());

        let (golf_id, golf_word) = word_list.add_word(
            &RawWordListEntry {
                normalized: "golf".into(),
                canonical: "golf".into(),
                score: 0,
            },
            false,
        );
        soft_dupe_index.add_word(golf_id, golf_word);

        let (golfy_id, golfy_word) = word_list.add_word(
            &RawWordListEntry {
                normalized: "golfy".into(),
                canonical: "golfy".into(),
                score: 0,
            },
            false,
        );
        soft_dupe_index.add_word(golfy_id, golfy_word);

        let (golves_id, golves_word) = word_list.add_word(
            &RawWordListEntry {
                normalized: "golves".into(),
                canonical: "golves".into(),
                score: 0,
            },
            false,
        );
        soft_dupe_index.add_word(golves_id, golves_word);

        let is_dupe = |index: &BoxedDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            index
                .get_dupes_by_length(id_1)
                .and_then(|dupes_by_length| dupes_by_length.get(&id_2.0).cloned())
                .map_or(false, |dupes| dupes.contains(&id_2.1))
        };

        let assert_dupe = |index: &BoxedDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            assert!(is_dupe(index, id_1, id_2));
            assert!(is_dupe(index, id_2, id_1));
        };

        let assert_not_dupe = |index: &BoxedDupeIndex, id_1: GlobalWordId, id_2: GlobalWordId| {
            assert!(!is_dupe(index, id_1, id_2));
            assert!(!is_dupe(index, id_2, id_1));
        };

        let main_index = &word_list.dupe_index.unwrap();

        // Regular index doesn't see any of these as dupes
        assert_not_dupe(main_index, (4, golf_id), (5, golfy_id));
        assert_not_dupe(main_index, (4, golf_id), (6, golves_id));
        assert_not_dupe(main_index, (5, golfy_id), (6, golves_id));

        // Secondary index sees golf/golfy as dupes but not golf/golves
        assert_dupe(&soft_dupe_index, (4, golf_id), (5, golfy_id));
        assert_not_dupe(&soft_dupe_index, (4, golf_id), (6, golves_id));
        assert_not_dupe(&soft_dupe_index, (5, golfy_id), (6, golves_id));

        // Add custom pair
        soft_dupe_index.add_dupe_pair((4, golf_id), (6, golves_id));

        // After customization, golf/golves is a dupe
        assert_dupe(&soft_dupe_index, (4, golf_id), (5, golfy_id));
        assert_dupe(&soft_dupe_index, (4, golf_id), (6, golves_id));
        assert_not_dupe(&soft_dupe_index, (5, golfy_id), (6, golves_id));

        // Remove custom pair and check again
        soft_dupe_index.remove_dupe_pair((4, golf_id), (6, golves_id));
        assert_not_dupe(&soft_dupe_index, (4, golf_id), (6, golves_id));
    }
}
