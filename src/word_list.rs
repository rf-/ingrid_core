use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Debug;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::SystemTime;
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

    /// If the word is currently not hidden, what is the index of the source that it came from? If
    /// the same word appears in multiple sources, this will be the highest-priority (i.e., lowest)
    /// one.
    pub source_index: Option<u16>,
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

#[derive(Debug, Clone)]
pub enum WordListError {
    InvalidPath(String),
    InvalidWord(String),
    InvalidScore(String),
}

impl fmt::Display for WordListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let string = match self {
            WordListError::InvalidPath(path) => format!("Can’t read file: “{path}”"),
            WordListError::InvalidWord(word) => {
                format!("Word list contains invalid word: “{word}”")
            }
            WordListError::InvalidScore(score) => {
                format!("Word list contains invalid score: “{score}”")
            }
        };
        write!(f, "{string}")
    }
}

/// Configuration describing a source of wordlist entries.
pub enum WordListSourceConfig {
    Memory {
        id: String,
        words: Vec<(String, i32)>,
    },
    File {
        id: String,
        path: OsString,
    },
    FileContents {
        id: String,
        contents: &'static str,
    },
}

impl WordListSourceConfig {
    /// The unique, persistent id of this word list.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            WordListSourceConfig::Memory { id, .. }
            | WordListSourceConfig::FileContents { id, .. }
            | WordListSourceConfig::File { id, .. } => id.clone(),
        }
    }

    /// The last file modification time for this word list, if applicable. If
    /// this returns `None` the list won't be checked for updates.
    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        match self {
            WordListSourceConfig::Memory { .. } | WordListSourceConfig::FileContents { .. } => None,
            WordListSourceConfig::File { path, .. } => fs::metadata(path).ok()?.modified().ok(),
        }
    }
}

#[derive(Debug)]
pub struct WordListSourceState {
    pub id: String,
    pub mtime: Option<SystemTime>,
    pub errors: Vec<WordListError>,
}

/// `WordListSourceState`s keyed by the `id` of the relevant source.
pub type WordListSourceStates = HashMap<String, WordListSourceState>;

/// A single word list entry.
#[allow(dead_code)]
struct RawWordListEntry {
    pub length: usize,
    pub normalized: String,
    pub canonical: String,
    pub score: i32,
    pub source_index: Option<u16>,
}

fn parse_word_list_file_contents(
    file_contents: &str,
    source_index: u16,
    errors: &mut Vec<WordListError>,
) -> Vec<RawWordListEntry> {
    file_contents
        .lines()
        .map_while(|line| {
            if errors.len() > 100 {
                return None;
            }

            let line_parts: Vec<_> = line.split(';').collect();

            let canonical = line_parts[0].trim().to_string();
            let normalized = normalize_word(&canonical);
            if normalized.is_empty() {
                errors.push(WordListError::InvalidWord(line_parts[0].into()));
                return Some(None);
            }

            let Ok(score) = (if line_parts.len() < 2 {
                Ok(50)
            } else {
                line_parts[1].trim().parse::<i32>()
            }) else {
                errors.push(WordListError::InvalidScore(line_parts[1].into()));
                return Some(None);
            };

            Some(Some(RawWordListEntry {
                length: normalized.chars().count(),
                normalized,
                canonical,
                score,
                source_index: Some(source_index),
            }))
        })
        .flatten()
        .collect()
}

fn load_words_from_source(
    source: &WordListSourceConfig,
    source_index: u16,
) -> (Vec<RawWordListEntry>, WordListSourceState) {
    let id = source.id();
    let mtime = source.modified();
    let mut errors = vec![];

    let entries = match source {
        WordListSourceConfig::Memory { words, .. } => words
            .iter()
            .cloned()
            .filter_map(|(canonical, score)| {
                let normalized = normalize_word(&canonical);
                if normalized.is_empty() {
                    errors.push(WordListError::InvalidWord(canonical));
                    return None;
                }

                Some(RawWordListEntry {
                    length: normalized.chars().count(),
                    normalized,
                    canonical,
                    score,
                    source_index: Some(source_index),
                })
            })
            .collect(),

        WordListSourceConfig::File { path, .. } => {
            if let Ok(contents) = fs::read_to_string(path) {
                parse_word_list_file_contents(&contents, source_index, &mut errors)
            } else {
                errors.push(WordListError::InvalidPath(path.to_string_lossy().into()));
                vec![]
            }
        }

        WordListSourceConfig::FileContents { contents, .. } => {
            parse_word_list_file_contents(contents, source_index, &mut errors)
        }
    };

    (entries, WordListSourceState { id, mtime, errors })
}

fn load_words_from_sources(
    sources: &[WordListSourceConfig],
) -> (Vec<RawWordListEntry>, WordListSourceStates) {
    fn hash_str(str: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        str.hash(&mut hasher);
        hasher.finish()
    }

    assert!(sources.len() < 2usize.pow(16), "Too many word list sources");

    let mut seen_words: HashSet<u64> = HashSet::new();
    let mut result = vec![];
    let mut states = HashMap::new();

    for (source_index, source) in sources.iter().enumerate() {
        let (words, source_state) = load_words_from_source(source, source_index as u16);
        for word in words {
            let hash = hash_str(&word.normalized);
            if seen_words.contains(&hash) {
                continue;
            }
            result.push(word);
            seen_words.insert(hash);
        }
        states.insert(source_state.id.clone(), source_state);
    }

    (result, states)
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

    /// The most recently-received word list sources, as an ordered list.
    pub source_configs: Vec<WordListSourceConfig>,

    /// The last seen state of each word list source, keyed by source id.
    pub source_states: HashMap<String, WordListSourceState>,
}

impl WordList {
    /// Construct a new `WordList` using the given sources (omitting any entries that are longer than
    /// `max_length`).
    #[allow(dead_code)]
    #[must_use]
    pub fn new(
        source_configs: Vec<WordListSourceConfig>,
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
            source_configs: vec![],
            source_states: HashMap::new(),
        };

        instance.replace_list(source_configs, max_length, false);

        instance
    }

    /// If the given normalized word is already in the list, return its id; if not, add it as a
    /// hidden entry and return the id of that.
    pub fn get_word_id_or_add_hidden(&mut self, normalized_word: &str) -> GlobalWordId {
        self.word_id_by_string
            .get(normalized_word)
            .copied()
            .map_or_else(
                || self.add_hidden_word(normalized_word),
                |word_id| (normalized_word.chars().count(), word_id),
            )
    }

    /// Borrow an existing word using its global id.
    #[must_use]
    pub fn get_word(&self, global_word_id: GlobalWordId) -> &Word {
        &self.words[global_word_id.0][global_word_id.1]
    }

    /// Add the given word to the list as a hidden entry and trigger the update callback. The word must not be part of the list yet.
    pub fn add_hidden_word(&mut self, normalized_word: &str) -> GlobalWordId {
        let global_word_id = self.add_word_silent(
            &RawWordListEntry {
                length: normalized_word.chars().count(),
                normalized: normalized_word.to_string(),
                canonical: normalized_word.to_string(),
                score: 0,
                source_index: None,
            },
            true,
        );

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
            source_index: raw_entry.source_index,
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
        source_configs: Vec<WordListSourceConfig>,
        max_length: Option<usize>,
        silent: bool,
    ) -> (bool, HashSet<GlobalWordId>) {
        self.source_configs = source_configs;
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

        let (raw_entries, source_states) = load_words_from_sources(&self.source_configs);
        self.source_states = source_states;

        // Now go through our new words and add them.
        for raw_entry in raw_entries {
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
                word.source_index = raw_entry.source_index;
                removed_words_set.remove(&(word_length, existing_word_id));
            } else if !silent {
                added_any_words = true;
                let added_word_id = self.add_word_silent(&raw_entry, false);
                added_words.push(added_word_id);
            } else {
                added_any_words = true;
                self.add_word_silent(&raw_entry, false);
            }
        }

        // Finally, hide any words that were in our existing list but aren't in the new one.
        for &(length, word_id) in &removed_words_set {
            self.words[length][word_id].hidden = true;
            self.words[length][word_id].source_index = None;
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
    /// every letter up front, because word list entries may also contain numbers, non-English
    /// letters, or punctuation.
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

    pub fn optimistically_update_word(&mut self, canonical: &str, score: i32, source_index: u16) {
        let normalized = normalize_word(canonical);
        if normalized.is_empty() {
            return;
        }

        let (length, word_id) = self.get_word_id_or_add_hidden(&normalized);
        let word = &mut self.words[length][word_id];
        let should_update = word
            .source_index
            .map_or(true, |existing_index| source_index <= existing_index);

        if should_update {
            word.canonical_string = canonical.into();
            word.score = score as f32;
            word.hidden = false;
            word.source_index = Some(source_index);
        }
    }

    /// For each source provided last time we loaded or updated, return any errors it emitted.
    #[must_use]
    pub fn get_source_errors(&self) -> HashMap<String, Vec<WordListError>> {
        let mut source_errors = HashMap::new();

        for (source_id, source_state) in &self.source_states {
            source_errors.insert(source_id.clone(), source_state.errors.clone());
        }

        source_errors
    }

    /// If any word lists have been modified since the last time we refreshed, return their ids.
    #[must_use]
    pub fn identify_stale_sources(&self) -> Vec<String> {
        self.source_configs
            .iter()
            .filter_map(|source_config| {
                let id = source_config.id();
                let old_mtime = self.source_states.get(&id).and_then(|state| state.mtime);
                let new_mtime = source_config.modified();

                if old_mtime.is_some() && new_mtime.is_some() && old_mtime != new_mtime {
                    Some(id)
                } else {
                    None
                }
            })
            .collect()
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
    use crate::word_list::{WordList, WordListSourceConfig};
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

    #[must_use]
    pub fn word_list_source_config() -> Vec<WordListSourceConfig> {
        vec![WordListSourceConfig::File {
            id: "0".into(),
            path: dictionary_path().into(),
        }]
    }

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    #[allow(clippy::float_cmp)]
    fn test_loads_words_up_to_max_length() {
        let word_list = WordList::new(word_list_source_config(), Some(5), None);

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

        assert!(word_list.word_id_by_string.get("skates").is_none());
    }

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    fn test_dynamic_max_length() {
        let mut word_list = WordList::new(word_list_source_config(), None, None);

        assert_eq!(word_list.max_length, None);
        assert_eq!(word_list.words.len(), 16);

        let &word_id = word_list
            .word_id_by_string
            .get("skates")
            .expect("word list should include 'skates'");

        let word = &word_list.words[6][word_id];
        assert_eq!(word.hidden, false);
        assert_eq!(word.source_index, Some(0));

        word_list.replace_list(word_list_source_config(), Some(5), false);

        assert_eq!(word_list.max_length, Some(5));
        assert_eq!(word_list.words.len(), 16);

        let word = &word_list.words[6][word_id];
        assert_eq!(word.hidden, true);
        assert_eq!(word.source_index, None);
    }

    #[test]
    #[allow(clippy::unicode_not_nfc)]
    fn test_unusual_characters() {
        let word_list = WordList::new(
            vec![WordListSourceConfig::Memory {
                id: "0".into(),
                words: vec![
                    // Non-English character expressed as one two-byte `char`
                    ("monsutâ".into(), 50),
                    // Non-English character expressed as two chars w/ combining form
                    ("hélen".into(), 50),
                ],
            }],
            None,
            None,
        );

        assert_eq!(
            word_list.words.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 1, 0, 1]
        );
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn test_soft_dupe_index() {
        let mut word_list = WordList::new(vec![], Some(6), Some(5));
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

        word_list.replace_list(
            vec![WordListSourceConfig::Memory {
                id: "0".into(),
                words: vec![
                    ("golf".into(), 0),
                    ("golfy".into(), 0),
                    ("golves".into(), 0),
                ],
            }],
            None,
            false,
        );

        let golf_id = (4, *word_list.word_id_by_string.get("golf").unwrap());
        let golfy_id = (5, *word_list.word_id_by_string.get("golfy").unwrap());
        let golves_id = (6, *word_list.word_id_by_string.get("golves").unwrap());

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

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    #[allow(clippy::float_cmp)]
    #[allow(clippy::too_many_lines)]
    fn test_source_management() {
        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::Memory {
                    id: "0".into(),
                    words: vec![("wolves".into(), 70), ("wolvvves".into(), 71)],
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        assert!(word_list.get_source_errors().get("0").unwrap().is_empty());
        assert!(word_list.get_source_errors().get("1").unwrap().is_empty());

        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let wolvvves_id = word_list.get_word_id_or_add_hidden("wolvvves");
        let wharves_id = word_list.get_word_id_or_add_hidden("wharves");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 70.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(0));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(1));
        }

        word_list.optimistically_update_word("wolves", 72, 0);
        word_list.optimistically_update_word("wharves", 73, 0);
        word_list.optimistically_update_word("worfs", 74, 0);

        let worfs_id = word_list.get_word_id_or_add_hidden("worfs");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 72.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 73.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 74.0);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(0));
        }

        word_list.replace_list(
            vec![
                WordListSourceConfig::File {
                    id: "1".into(),
                    path: dictionary_path().into(),
                },
                WordListSourceConfig::Memory {
                    id: "0".into(),
                    words: vec![("wolves".into(), 70), ("wolvvves".into(), 71)],
                },
            ],
            None,
            false,
        );

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.hidden, true);
            assert_eq!(worfs.source_index, None);
        }

        word_list.optimistically_update_word("wolves", 80, 1);
        word_list.optimistically_update_word("wolvvves", 81, 1);
        word_list.optimistically_update_word("wharves", 82, 1);
        word_list.optimistically_update_word("worfs", 83, 1);

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 81.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 83.0);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(1));
        }
    }
}
