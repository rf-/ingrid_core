use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Debug;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::SystemTime;
use std::{fmt, fs, io, mem};
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

    /// Does the word also appear in any lower-priority lists than the one it came from?
    pub shadowed: bool,
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
#[derive(Debug, Clone)]
pub enum WordListSourceConfig {
    Memory {
        id: String,
        enabled: bool,
        words: Vec<(String, i32)>,
    },
    File {
        id: String,
        enabled: bool,
        path: OsString,
    },
    FileContents {
        id: String,
        enabled: bool,
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

    /// Whether this list will affect `words`. Lists that aren't enabled don't even add hidden
    /// entries, but can still be edited and saved.
    #[must_use]
    pub fn enabled(&self) -> bool {
        match self {
            WordListSourceConfig::Memory { enabled, .. }
            | WordListSourceConfig::FileContents { enabled, .. }
            | WordListSourceConfig::File { enabled, .. } => *enabled,
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

/// A word list change waiting to be persisted.
#[derive(Debug, Clone)]
pub enum PendingWordListUpdate {
    AddOrUpdate { canonical: String, score: i32 },
    Delete,
}

#[derive(Debug)]
pub struct WordListSourceState {
    pub source_index: u16,
    pub id: String,
    pub mtime: Option<SystemTime>,
    pub errors: Vec<WordListError>,
    pub pending_updates: HashMap<String, PendingWordListUpdate>,
}

/// `WordListSourceState`s keyed by the `id` of the relevant source.
pub type WordListSourceStates = HashMap<String, WordListSourceState>;

/// A single word list entry.
#[allow(dead_code)]
#[derive(Debug)]
pub struct RawWordListEntry {
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

#[must_use]
pub fn load_words_from_source(
    source: &WordListSourceConfig,
    source_index: u16,
) -> (Vec<RawWordListEntry>, WordListSourceState) {
    let id = source.id();
    let mtime = source.modified();
    let mut errors = vec![];

    let entries = match source {
        WordListSourceConfig::Memory { .. }
        | WordListSourceConfig::File { .. }
        | WordListSourceConfig::FileContents { .. }
            if !source.enabled() =>
        {
            vec![]
        }

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

    (
        entries,
        WordListSourceState {
            source_index,
            id,
            mtime,
            errors,
            pending_updates: HashMap::new(),
        },
    )
}

type OnUpdateCallback = Box<dyn FnMut(&mut WordList, &[GlobalWordId]) + Send + Sync>;

/// How urgently we need to sync to disk, if at all. `LowPriority` means we have changes that
/// exist only in memory; `HighPriority` means `words` may actually be incorrect until we sync.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncState {
    Synced,
    LowPriority,
    HighPriority,
}

/// Errors that can arise when syncing to disk, keyed by the relevant source id.
pub type SyncErrors = HashMap<String, io::Error>;

/// Result of a call to `optimistically_delete_word`.
#[derive(Debug, PartialEq, Eq)]
pub enum OptimisticDeletionResult {
    /// We didn't modify `words` because there was a higher-priority entry.
    NoChange,
    /// We modified `words` and `shadowed` was false, so we don't need to reload.
    CleanDeletion,
    /// We modified `words` and `shadowed` was true, so we need to reload to ensure we aren't
    /// missing a relevant lower-priority entry.
    PossiblyIncorrectDeletion,
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
    pub dupe_index: BoxedDupeIndex,

    /// The maximum word length provided when configuring the WordList, if any.
    pub max_length: Option<usize>,

    /// Callback run after adding words.
    pub on_update: Option<OnUpdateCallback>,

    /// The most recently-received word list sources, as an ordered list.
    pub source_configs: Vec<WordListSourceConfig>,

    /// The last seen state of each word list source, keyed by source id.
    pub source_states: HashMap<String, WordListSourceState>,

    /// Flag for whether we need to sync with disk.
    pub sync_state: SyncState,
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
            sync_state: SyncState::Synced,
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
    fn add_hidden_word(&mut self, normalized_word: &str) -> GlobalWordId {
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
            on_update(self, &[global_word_id]);
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
            shadowed: false,
        });

        self.word_id_by_string
            .insert(raw_entry.normalized.clone(), word_id);

        self.dupe_index
            .add_word(word_id, &self.words[word_length][word_id]);

        (word_length, word_id)
    }

    /// Refresh the word list from the given sources. This fully replaces the list from an
    /// outside perspective, although we don't actually remove any words since we want to
    /// keep word ids stable in case they're referenced elsewhere.
    ///
    /// There are two separate mechanisms for tracking changes, with different semantics:
    ///
    /// - We return a tuple of a boolean indicating whether any words have either been added,
    ///   unhidden, or increased in score and a set of the ids of words that have either been
    ///   hidden or have reduced their score.
    ///
    /// - If `silent` is false and an `on_update` callback is present, we track
    ///   any words that are added to the list (independent of hidden status and score) and
    ///   pass them into the callback.
    pub fn replace_list(
        &mut self,
        source_configs: Vec<WordListSourceConfig>,
        max_length: Option<usize>,
        silent: bool,
    ) -> (bool, HashSet<GlobalWordId>) {
        self.source_configs = source_configs;
        self.max_length = max_length;

        // Start with the assumption that we're removing everything. This is for tracking
        // which words we haven't seen yet; we'll fill in `less_visible_words_set` more
        // selectively to be returned to the caller.
        let mut removed_words_set: HashSet<GlobalWordId> = self
            .words
            .iter()
            .enumerate()
            .flat_map(|(length, words)| {
                words.iter().enumerate().filter_map(move |(word_id, word)| {
                    if word.hidden {
                        None
                    } else {
                        Some((length, word_id))
                    }
                })
            })
            .collect();
        let mut less_visible_words_set: HashSet<GlobalWordId> = HashSet::new();

        let mut newly_added_words: Vec<GlobalWordId> = vec![];
        let mut any_more_visible = false;
        let silent = silent || self.on_update.is_none();

        // If we were given a max length, make sure we have that many buckets.
        if let Some(max_length) = max_length {
            while self.words.len() < max_length + 1 {
                self.words.push(vec![]);
            }
        }

        self.load_words_from_source_configs(
            max_length,
            |word_list, raw_entry| {
                let word_length = raw_entry.length;
                let existing_word_id = word_list.word_id_by_string.get(&raw_entry.normalized);

                if let Some(&existing_word_id) = existing_word_id {
                    let word = &mut word_list.words[word_length][existing_word_id];
                    if word.hidden || raw_entry.score as f32 > word.score {
                        any_more_visible = true;
                    }
                    if !word.hidden && (raw_entry.score as f32) < word.score {
                        less_visible_words_set.insert((word_length, existing_word_id));
                    }
                    word.score = raw_entry.score as f32;
                    word.hidden = false;
                    word.canonical_string = raw_entry.canonical.clone();
                    word.source_index = raw_entry.source_index;
                    word.shadowed = false;
                    removed_words_set.remove(&(word_length, existing_word_id));
                } else if !silent {
                    any_more_visible = true;
                    let added_word_id = word_list.add_word_silent(&raw_entry, false);
                    newly_added_words.push(added_word_id);
                } else {
                    any_more_visible = true;
                    word_list.add_word_silent(&raw_entry, false);
                }
            },
            |word_list, raw_entry| {
                let existing_word_id = word_list.word_id_by_string.get(&raw_entry.normalized);

                if let Some(&existing_word_id) = existing_word_id {
                    let word = &mut word_list.words[raw_entry.length][existing_word_id];
                    word.shadowed = true;
                } else {
                    #[cfg(feature = "check_invariants")]
                    panic!("replace_list: called mark_as_shadowed without existing word");
                }
            },
        );

        // Finally, hide any words that were in our existing list but aren't in the new one.
        for &(length, word_id) in &removed_words_set {
            self.words[length][word_id].hidden = true;
            self.words[length][word_id].source_index = None;
            self.words[length][word_id].shadowed = false;
            less_visible_words_set.insert((length, word_id));
        }

        if let Some(mut on_update) = self.on_update.take() {
            on_update(self, &newly_added_words);
            self.on_update = Some(on_update);
        }

        (any_more_visible, less_visible_words_set)
    }

    fn load_words_from_source_configs(
        &mut self,
        max_length: Option<usize>,
        mut add_word: impl FnMut(&mut WordList, RawWordListEntry),
        mut mark_as_shadowed: impl FnMut(&mut WordList, RawWordListEntry),
    ) {
        fn hash_str(str: &str) -> u64 {
            let mut hasher = DefaultHasher::new();
            str.hash(&mut hasher);
            hasher.finish()
        }

        let source_configs = mem::take(&mut self.source_configs);
        let mut source_states = mem::take(&mut self.source_states);
        assert!(
            source_configs.len() < 2usize.pow(16),
            "Too many word list sources"
        );

        let mut seen_words: HashSet<u64> = HashSet::new();
        let mut new_states = HashMap::new();

        for (source_index, source) in source_configs.iter().enumerate() {
            let (words, mut new_state) = load_words_from_source(source, source_index as u16);

            // Any pending updates should be carried over from the old state to the new state.
            let old_state = source_states.remove(&new_state.id);
            if let Some(old_state) = old_state {
                new_state.pending_updates = old_state.pending_updates;
            }

            // If the source is disabled, none of its words (or pending updates) should affect the
            // actual wordlist.
            if !source.enabled() {
                new_states.insert(new_state.id.clone(), new_state);
                continue;
            }

            let mut words_iter: Box<dyn Iterator<Item = RawWordListEntry>> =
                Box::new(words.into_iter());

            // If there are any pending updates, they take priority over the list items as stored.
            let mut updated_words: Vec<RawWordListEntry> = vec![];
            if !new_state.pending_updates.is_empty() {
                let mut superseded_words = HashSet::new();

                for (normalized, pending_update) in &new_state.pending_updates {
                    superseded_words.insert(normalized.clone());

                    if let PendingWordListUpdate::AddOrUpdate { canonical, score } = pending_update
                    {
                        updated_words.push(RawWordListEntry {
                            length: normalized.chars().count(),
                            normalized: normalized.clone(),
                            canonical: canonical.clone(),
                            score: *score,
                            source_index: Some(source_index as u16),
                        });
                    }
                }

                words_iter = Box::new(
                    words_iter.filter(move |word| !superseded_words.contains(&word.normalized)),
                );
            }

            for word in updated_words.into_iter().chain(words_iter) {
                if let Some(max_length) = max_length {
                    if word.length > max_length {
                        continue;
                    }
                }
                let hash = hash_str(&word.normalized);
                if seen_words.contains(&hash) {
                    // Technically this is wrong, since a word could be shadowed by a duplicate
                    // entry in the same wordlist file, but it's not a big deal.
                    mark_as_shadowed(self, word);
                    continue;
                }
                add_word(self, word);
                seen_words.insert(hash);
            }

            new_states.insert(new_state.id.clone(), new_state);
        }

        self.source_configs = source_configs;
        self.source_states = new_states;
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

    /// Update the word list state to be consistent with the given word being upserted into
    /// the given source. This never requires a refresh, since we either add a
    /// previously-unknown word, update or shadow an existing definition, or just ignore the
    /// update if it would be shadowed.
    pub fn optimistically_update_word(&mut self, canonical: &str, score: i32, source_id: &str) {
        let normalized = normalize_word(canonical);
        if normalized.is_empty() {
            return;
        }

        let Some(source_index) = self.find_source_index_for_id(source_id) else {
            return;
        };
        let source_config = &self.source_configs[source_index as usize];

        // Regardless of whether this change is visible in `words`, we need to buffer it
        // to be persisted to the file.
        let source_id = source_config.id();
        if let Some(source_state) = self.source_states.get_mut(&source_id) {
            source_state.pending_updates.insert(
                normalized.clone(),
                PendingWordListUpdate::AddOrUpdate {
                    canonical: canonical.into(),
                    score,
                },
            );
        } else {
            panic!("optimistically_update_word: no source state found for id");
        }
        if self.sync_state == SyncState::Synced {
            self.sync_state = SyncState::LowPriority;
        }

        // If the source is disabled, we don't need to update the actual wordlist.
        if !source_config.enabled() {
            return;
        }

        let (length, word_id) = self.get_word_id_or_add_hidden(&normalized);
        let word = &mut self.words[length][word_id];
        let should_update = word
            .source_index
            .map_or(true, |existing_index| source_index <= existing_index);

        if !should_update {
            word.shadowed = true;
            return;
        }

        let shadowed = word.shadowed
            || word
                .source_index
                .map_or(false, |existing_index| source_index < existing_index);

        word.canonical_string = canonical.into();
        word.score = score as f32;
        word.hidden = false;
        word.source_index = Some(source_index);
        word.shadowed = shadowed;
    }

    /// Update the word list state to be consistent with the given word being deleted from
    /// the given source.
    pub fn optimistically_delete_word(
        &mut self,
        normalized: &str,
        source_id: &str,
    ) -> OptimisticDeletionResult {
        let Some(source_index) = self.find_source_index_for_id(source_id) else {
            return OptimisticDeletionResult::NoChange;
        };
        let source_config = &self.source_configs[source_index as usize];

        // Regardless of whether this change is visible in `words`, we need to buffer it
        // to be persisted to the file.
        let source_id = source_config.id();
        if let Some(source_state) = self.source_states.get_mut(&source_id) {
            source_state
                .pending_updates
                .insert(normalized.to_string(), PendingWordListUpdate::Delete);
        } else {
            panic!("optimistically_delete_word: no source state found for id");
        }
        if self.sync_state == SyncState::Synced {
            self.sync_state = SyncState::LowPriority;
        }

        // If the source is disabled, we don't need to update the actual wordlist.
        if !source_config.enabled() {
            return OptimisticDeletionResult::NoChange;
        }

        // Now we can mark it as hidden, but if it was shadowed we need to refresh since
        // hiding it may have been incorrect.
        let length = normalized.chars().count();
        let Some(&word_id) = self.word_id_by_string.get(normalized) else {
            #[cfg(feature = "check_invariants")]
            panic!("optimistically_delete_word_impl: word doesn't exist");
            #[cfg(not(feature = "check_invariants"))]
            return OptimisticDeletionResult::NoChange;
        };

        let word = &mut self.words[length][word_id];

        // If the version we have stored isn't from this list, no action is needed --
        // either it's not present in the list at all, or it's already shadowed by a
        // higher-priority source.
        if word.source_index != Some(source_index) {
            return OptimisticDeletionResult::NoChange;
        }

        let was_shadowed = word.shadowed;
        word.hidden = true;
        word.source_index = None;
        word.shadowed = false;

        if was_shadowed {
            if self.sync_state != SyncState::HighPriority {
                self.sync_state = SyncState::HighPriority;
            }
            OptimisticDeletionResult::PossiblyIncorrectDeletion
        } else {
            OptimisticDeletionResult::CleanDeletion
        }
    }

    fn find_source_index_for_id(&self, source_id: &str) -> Option<u16> {
        self.source_configs
            .iter()
            .enumerate()
            .find_map(|(index, config)| {
                if config.id() == source_id {
                    Some(index as u16)
                } else {
                    None
                }
            })
    }

    /// If there are any pending updates, write them to disk. Return a flag indicating whether we
    /// need to refresh the word lists. If any pending updates can't be written (probably due to
    /// something like permissions issues or a drive not being mounted), return error info, reset
    /// `sync_state` to `Synced`, and keep the pending updates in place.
    pub fn sync_updates_to_disk(&mut self) -> (bool, SyncErrors) {
        let mut should_refresh = self.sync_state == SyncState::HighPriority;
        let mut sync_errors: SyncErrors = HashMap::new();

        // For each source file, write any pending updates.
        for source_state in self.source_states.values_mut() {
            if source_state.pending_updates.is_empty() {
                continue;
            }

            let source_config = &self.source_configs[source_state.source_index as usize];

            // If the file was updated externally since the last time we reloaded it, there's no
            // problem with this process since we'd read it from disk either way, but unless it's
            // disabled we'll definitely need to do a full refresh of the word list when we're done
            // syncing.
            if source_config.enabled() && source_state.mtime != source_config.modified() {
                should_refresh = true;
            }

            let WordListSourceConfig::File { path, .. } = &source_config else {
                panic!("sync_updates_to_disk: non-file config had pending changes");
            };
            let path = path.clone();
            let mut pending_updates = source_state.pending_updates.clone();

            // Load the current file contents. Apply the pending changes inline to words
            // that are already in the list, then append any new words to the end.
            // We don't want to use `parse_word_list_file_contents` here since that
            // prioritizes ending up with a clean list and parse errors, whereas here we
            // want to be as unopinionated as possible.
            let file_contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // If the file doesn't exist, we can just treat it as empty.
                    String::new()
                }
                Err(err) => {
                    sync_errors.insert(source_state.id.clone(), err);
                    continue;
                }
            };

            // If we have file contents, check whether the first line ending is
            // Windows-style or Unix-style and follow suit. Otherwise, use Windows line
            // endings on Windows.
            let use_windows_line_endings = {
                let first_n = file_contents.find('\n');
                if let Some(first_n) = first_n {
                    first_n > 0 && file_contents.as_bytes()[first_n - 1] == b'\r'
                } else {
                    cfg!(target_os = "windows")
                }
            };

            // For each line, either omit it if the user deleted the word, replace its
            // canonical string and score if the user updated the word, or leave it as is.
            // Leave any extra semicolon-separated fields alone.
            let mut modified_lines: Vec<String> = file_contents
                .lines()
                .filter_map(|line| {
                    let mut line_parts: Vec<String> = line.split(';').map(str::to_string).collect();
                    let normalized = normalize_word(&line_parts[0]);

                    if pending_updates.get(&normalized).is_none() {
                        return Some(line.to_string());
                    }

                    // We have to do this the ugly way since otherwise we'd have to choose
                    // whether to remove the update or not before we know what type it is,
                    // and we want to leave deletions in place so they apply to any number
                    // of entries in the file.
                    if matches!(
                        pending_updates.get(&normalized),
                        Some(PendingWordListUpdate::Delete)
                    ) {
                        return None;
                    }

                    if let Some(PendingWordListUpdate::AddOrUpdate { canonical, score }) =
                        pending_updates.remove(&normalized)
                    {
                        line_parts[0] = canonical;
                        if line_parts.len() < 2 {
                            line_parts.push(score.to_string());
                        } else {
                            line_parts[1] = score.to_string();
                        }
                        return Some(line_parts.join(";"));
                    }

                    unreachable!()
                })
                .collect();

            // Since we already removed the updates corresponding to any words that
            // already existed, what's left is words we should append to the bottom.
            for pending_update in pending_updates.values() {
                if let PendingWordListUpdate::AddOrUpdate { canonical, score } = pending_update {
                    modified_lines.push([canonical.as_str(), score.to_string().as_str()].join(";"));
                }
            }

            let mut new_file_contents = String::new();
            for line in modified_lines {
                new_file_contents.push_str(&line);
                if use_windows_line_endings {
                    new_file_contents.push('\r');
                }
                new_file_contents.push('\n');
            }

            // For safety, first write to a temporary file and then rename it over the
            // destination file.
            let mut temp_path = path.clone();
            temp_path.push(".tmp");
            if let Err(err) = fs::write(&temp_path, new_file_contents) {
                sync_errors.insert(source_state.id.clone(), err);
                continue;
            }
            if let Err(err) = fs::rename(&temp_path, &path) {
                sync_errors.insert(source_state.id.clone(), err);
                continue;
            }

            // Now that we've updated the file, store its new mtime and clear the pending
            // updates. Note that technically this is racy since the file could have been
            // written by someone else between when we wrote it and checked the mtime. If
            // this happened, we could miss those updates until the file is written again
            // or the user makes more modifications.
            source_state.mtime = source_config.modified();
            source_state.pending_updates = HashMap::new();
        }

        // Regardless of whether we actually succeeded in syncing everything, we should
        // reset this flag so that we don't end up accidentally infinite-looping. Calling
        // `sync_updates_to_disk` again will still retry and files that failed, since the
        // pending updates are still present.
        self.sync_state = SyncState::Synced;

        (should_refresh, sync_errors)
    }

    /// Reload the word list(s) using the current config. Returns the same information as
    /// `replace_list`.
    pub fn refresh_from_disk(&mut self) -> (bool, HashSet<GlobalWordId>) {
        self.replace_list(self.source_configs.clone(), self.max_length, false)
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
#[allow(clippy::bool_assert_comparison)]
#[allow(clippy::float_cmp)]
#[allow(clippy::too_many_lines)]
#[allow(clippy::similar_names)]
pub mod tests {
    use crate::dupe_index::{AnyDupeIndex, DupeIndex};
    use crate::types::GlobalWordId;
    use crate::word_list::{OptimisticDeletionResult, SyncState, WordList, WordListSourceConfig};
    use std::collections::HashSet;
    use std::fs;
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
            enabled: true,
            path: dictionary_path().into(),
        }]
    }

    #[test]
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
                enabled: true,
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
    fn test_soft_dupe_index() {
        let mut word_list = WordList::new(vec![], Some(6), Some(5));
        let mut soft_dupe_index = DupeIndex::<4>::default();

        // This doesn't do anything except make sure it's OK to call this method unboxed
        word_list.populate_dupe_index(&mut soft_dupe_index);

        let soft_dupe_index = Arc::new(Mutex::new(soft_dupe_index));

        word_list.on_update = {
            let soft_dupe_index = soft_dupe_index.clone();

            Some(Box::new(move |word_list, added_word_ids| {
                let mut soft_dupe_index = soft_dupe_index.lock().unwrap();

                for &(word_length, word_id) in added_word_ids {
                    soft_dupe_index.add_word(word_id, &word_list.words[word_length][word_id]);
                }
            }))
        };

        word_list.replace_list(
            vec![WordListSourceConfig::Memory {
                id: "0".into(),
                enabled: true,
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
    fn test_source_management() {
        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::Memory {
                    id: "0".into(),
                    enabled: true,
                    words: vec![("wolves".into(), 70), ("wolvvves".into(), 71)],
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
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
            assert_eq!(wolves.shadowed, true);
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(0));
            assert_eq!(wolvvves.shadowed, false);
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(1));
            assert_eq!(wharves.shadowed, false);
        }

        word_list.optimistically_update_word("wolves", 72, "0");
        word_list.optimistically_update_word("wharves", 73, "0");
        word_list.optimistically_update_word("worfs", 74, "0");

        let worfs_id = word_list.get_word_id_or_add_hidden("worfs");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 72.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
            assert_eq!(wolves.shadowed, true);
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 73.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
            assert_eq!(wharves.shadowed, true);
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 74.0);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(0));
            assert_eq!(worfs.shadowed, false);
        }

        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            OptimisticDeletionResult::PossiblyIncorrectDeletion
        );
        assert_eq!(
            word_list.optimistically_delete_word("worfs", "0"),
            OptimisticDeletionResult::CleanDeletion
        );

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.hidden, true);
            assert_eq!(wolves.source_index, None);
            assert_eq!(wolves.shadowed, false);
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.hidden, true);
            assert_eq!(worfs.source_index, None);
            assert_eq!(worfs.shadowed, false);
        }

        word_list.replace_list(
            vec![
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
                WordListSourceConfig::Memory {
                    id: "0".into(),
                    enabled: true,
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
            assert_eq!(wolves.shadowed, false);
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
            assert_eq!(wolvvves.shadowed, false);
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
            assert_eq!(wharves.shadowed, true); // optimistic update is still there
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.hidden, true);
            assert_eq!(worfs.source_index, None);
            assert_eq!(worfs.shadowed, false);
        }

        word_list.optimistically_update_word("wolves", 80, "0");
        word_list.optimistically_update_word("wolvvves", 81, "0");
        word_list.optimistically_update_word("wharves", 82, "0");
        word_list.optimistically_update_word("worfs", 83, "0");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
            assert_eq!(wolves.shadowed, true);
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 81.0);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
            assert_eq!(wolvvves.shadowed, false);
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50.0);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
            assert_eq!(wharves.shadowed, true);
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 83.0);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(1));
            assert_eq!(worfs.shadowed, false);
        }
    }

    #[test]
    fn test_word_list_updating() {
        let tmpfile_1 = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile_1.path(), "wolves;51\nsteev;54\nijsnzlsj;20\n").unwrap();

        let tmpfile_2 = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile_2.path(), "wolves;52\r\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile_1.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
                WordListSourceConfig::File {
                    id: "2".into(),
                    enabled: true,
                    path: tmpfile_2.path().into(),
                },
            ],
            None,
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");
        let what_id = word_list.get_word_id_or_add_hidden("what");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 51.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
            assert_eq!(wolves.shadowed, true);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 54.0);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
            assert_eq!(steev.shadowed, false);
        }
        {
            let what = word_list.get_word(what_id);
            assert_eq!(what.score, 50.0);
            assert_eq!(what.hidden, false);
            assert_eq!(what.source_index, Some(1));
            assert_eq!(what.shadowed, false);
        }

        word_list.optimistically_update_word("wOl vEs", 60, "0");
        word_list.optimistically_delete_word("steev", "0");
        word_list.optimistically_update_word("Wolves", 99, "2");
        word_list.optimistically_update_word("tonberry", 30, "2");
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wOl vEs");
            assert_eq!(wolves.score, 60.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
            assert_eq!(wolves.shadowed, true);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 54.0);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
            assert_eq!(steev.shadowed, false);
        }
        {
            let tonberry_id = word_list.get_word_id_or_add_hidden("tonberry");
            let tonberry = word_list.get_word(tonberry_id);
            assert_eq!(tonberry.score, 30.0);
            assert_eq!(tonberry.hidden, false);
            assert_eq!(tonberry.source_index, Some(2));
            assert_eq!(tonberry.shadowed, false);
        }
        assert_eq!(word_list.sync_state, SyncState::LowPriority);

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(!should_refresh);
        assert!(sync_errors.is_empty());

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
        assert!(less_visible_words.is_empty());

        assert_eq!(
            fs::read_to_string(tmpfile_1.path()).unwrap(),
            "wOl vEs;60\nijsnzlsj;20\n"
        );
        assert_eq!(
            fs::read_to_string(tmpfile_2.path()).unwrap(),
            "Wolves;99\r\ntonberry;30\r\n"
        );
    }

    #[test]
    fn test_word_list_handle_shadowed_deletion() {
        let tmpfile_1 = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile_1.path(), "wolves;51\nsteev;54\nijsnzlsj;20\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile_1.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 51.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
            assert_eq!(wolves.shadowed, true);
        }

        // Since "wolves" is shadowed, if we delete it we'll be in an inconsistent state,
        // so the `sync_state` needs to reflect that.
        word_list.optimistically_delete_word("wolves", "0");
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 51.0);
            assert_eq!(wolves.hidden, true);
            assert_eq!(wolves.source_index, None);
            assert_eq!(wolves.shadowed, false);
        }
        assert_eq!(word_list.sync_state, SyncState::HighPriority);

        // If we then sync to disk, it will also refresh the list, which will be reflected
        // in `any_more_visible` since it's undoing the optimistic update.
        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert!(sync_errors.is_empty());

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(any_more_visible);
        assert_eq!(less_visible_words, HashSet::new());

        assert_eq!(
            fs::read_to_string(tmpfile_1.path()).unwrap(),
            "steev;54\nijsnzlsj;20\n"
        );
    }

    #[test]
    fn test_word_list_handle_replace_with_pending_changes() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile.path(), "wolves;51\nsteev;54\nijsnzlsj;20\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // See previous test
        word_list.optimistically_delete_word("wolves", "0");
        word_list.optimistically_update_word("Steev", 55, "0");
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 51.0);
            assert_eq!(wolves.hidden, true);
            assert_eq!(wolves.source_index, None);
            assert_eq!(wolves.shadowed, false);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.canonical_string, "Steev");
            assert_eq!(steev.score, 55.0);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
            assert_eq!(steev.shadowed, false);
        }
        assert_eq!(word_list.sync_state, SyncState::HighPriority);

        // This time, we want to refresh the list before we sync the updates. Then ensure
        // that we're still ready to sync and have still applied the updates on top of the
        // new file contents.
        fs::write(tmpfile.path(), "wolves;52\nsteev;48\nhejjira;20\n").unwrap();
        let (any_more_visible, less_visible_words) = word_list.replace_list(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            false,
        );
        assert!(any_more_visible);
        assert_eq!(
            less_visible_words,
            HashSet::from([word_list.get_word_id_or_add_hidden("ijsnzlsj")])
        );
        assert_eq!(word_list.sync_state, SyncState::HighPriority);
        {
            // The wolves change is actually applied correctly now, as a bonus.
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
            assert_eq!(wolves.shadowed, false);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.canonical_string, "Steev");
            assert_eq!(steev.score, 55.0);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
            assert_eq!(steev.shadowed, false);
        }

        // Make another out-of-band change, so we can validate that it gets loaded again
        // when we sync.
        fs::write(tmpfile.path(), "wolves;53\nsteev;47\nabcdedcba;22").unwrap();

        // Now we can sync, merging the in-memory changes with our out-of-band changes.
        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert!(sync_errors.is_empty());
        assert_eq!(word_list.sync_state, SyncState::Synced);

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(any_more_visible);
        assert_eq!(
            less_visible_words,
            HashSet::from([word_list.get_word_id_or_add_hidden("hejjira")])
        );

        assert_eq!(
            fs::read_to_string(tmpfile.path()).unwrap(),
            "Steev;55\nabcdedcba;22\n"
        );
    }

    #[test]
    fn test_word_list_handle_disk_sync_error() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile.path(), "wolves;51\nsteev;54\nidkidkidk;10\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        let idkidkidk_id = word_list.get_word_id_or_add_hidden("idkidkidk");

        // See previous tests
        word_list.optimistically_delete_word("wolves", "0");
        word_list.optimistically_update_word("Steev", 55, "0");
        assert_eq!(word_list.sync_state, SyncState::HighPriority);

        // If we replace the tempfile with a directory, it will be impossible to sync to
        // disk.
        fs::remove_file(tmpfile.path()).unwrap();
        fs::create_dir(tmpfile.path()).unwrap();

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert_eq!(
            sync_errors.get("0").unwrap().to_string(),
            "Is a directory (os error 21)"
        );
        assert_eq!(word_list.sync_state, SyncState::Synced);

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(any_more_visible); // the shadowed wolves reappearing
        assert_eq!(less_visible_words, HashSet::from([idkidkidk_id]));

        // Now, if we remove the directory, we should be able to write the updates to a
        // new file in the same location. Note that `idkidkidk` is gone though, since we
        // always prioritize the current state of the file over what we loaded before, and
        // now it's empty.
        fs::remove_dir(tmpfile.path()).unwrap();

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert!(sync_errors.is_empty());
        assert_eq!(word_list.sync_state, SyncState::Synced);

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
        assert_eq!(less_visible_words, HashSet::new());

        assert_eq!(fs::read_to_string(tmpfile.path()).unwrap(), "Steev;55\n");
    }

    #[test]
    fn test_word_list_update_disabled_list() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile.path(), "wolves;51\nsteev;54\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: false,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // The disabled list doesn't affect the actual word list.
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
            assert_eq!(wolves.shadowed, false);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
        }

        // Optimistic updates elevate the sync state but still don't affect the actual word list.
        word_list.optimistically_delete_word("wolves", "0");
        word_list.optimistically_update_word("sT eev", 51, "0");
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
            assert_eq!(wolves.shadowed, false);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
        }
        // The priority is low even though we deleted a word that appears in a lower-priority
        // source, since it wasn't actually shadowed, since we didn't get it from this list.
        assert_eq!(word_list.sync_state, SyncState::LowPriority);

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(!should_refresh);
        assert!(sync_errors.is_empty());

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
        assert!(less_visible_words.is_empty());

        assert_eq!(fs::read_to_string(tmpfile.path()).unwrap(), "sT eev;51\n");
    }

    #[test]
    fn test_word_list_update_disabled_list_and_reenable() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmpfile.path(), "wolves;51\nsteev;54\n").unwrap();

        let mut word_list = WordList::new(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: false,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // This is like the last one, except that between updating the list and syncing it we
        // re-enable it. This applies the buffered changes to the resulting word list.
        word_list.optimistically_delete_word("wolves", "0");
        word_list.optimistically_update_word("sT eev", 51, "0");
        assert_eq!(word_list.sync_state, SyncState::LowPriority);

        let (any_more_visible, less_visible_words) = word_list.replace_list(
            vec![
                WordListSourceConfig::File {
                    id: "0".into(),
                    enabled: true,
                    path: tmpfile.path().into(),
                },
                WordListSourceConfig::File {
                    id: "1".into(),
                    enabled: true,
                    path: dictionary_path().into(),
                },
            ],
            None,
            false,
        );
        assert!(any_more_visible); // steev
        assert!(less_visible_words.is_empty());
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50.0);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
            assert_eq!(wolves.shadowed, false);
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 51.0);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
            assert_eq!(steev.shadowed, false);
        }

        // Now do the actual sync.
        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(!should_refresh);
        assert!(sync_errors.is_empty());

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
        assert!(less_visible_words.is_empty());

        assert_eq!(fs::read_to_string(tmpfile.path()).unwrap(), "sT eev;51\n");
    }
}
