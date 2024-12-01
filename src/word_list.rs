use lazy_static::lazy_static;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::time::SystemTime;
use std::{fmt, fs, io, mem};
use unicode_normalization::UnicodeNormalization;

use crate::dupe_index::{AnyDupeIndex, BoxedDupeIndex, DupeIndex};
use crate::types::{GlobalWordId, GlyphId, WordId};
use crate::MAX_SLOT_LENGTH;

lazy_static! {
    /// Completely arbitrary mapping from letter to point value.
    static ref LETTER_POINTS: HashMap<char, u16> = {
        let chars_and_scores: Vec<(&str, u16)> = vec![
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

    /// The word's score, usually on a roughly 0 - 100 scale where 50 means average quality.
    pub score: u16,

    /// The sum of the scores of the word's letters.
    pub letter_score: u16,

    /// Is this word currently invisible to the user and unavailable for autofill? This will be
    /// true for non-words that are part of an input grid or for words that have been removed from
    /// the list dynamically.
    pub hidden: bool,

    /// If the word is currently not hidden, what is the index of the source that it came from? If
    /// the same word appears in multiple sources, this will be the highest-priority (i.e., lowest)
    /// one.
    pub source_index: Option<u16>,

    // If we specified a personal list in config, the score from that list.
    pub personal_word_score: Option<u16>,
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
        words: Vec<(String, u16)>,
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
    AddOrUpdate { canonical: String, score: u16 },
    Delete,
}

pub struct WordListSourceState {
    pub source_index: u16,
    pub id: String,
    pub entries: Vec<RawWordListEntry>,
    pub mtime: Option<SystemTime>,
    pub index: HashMap<String, usize>,
    pub errors: Vec<WordListError>,
    pub pending_updates: HashMap<String, PendingWordListUpdate>,
}

impl Debug for WordListSourceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WordList")
            .field("source_index", &self.source_index)
            .field("id", &self.id)
            .field("entries", &format!("({} entries)", self.entries.len()))
            .field("mtime", &self.mtime)
            .field("index", &format!("({} entries)", self.index.len()))
            .field("errors", &self.errors)
            .field("pending_updates", &self.pending_updates)
            .finish()
    }
}

impl WordListSourceState {
    fn get_entry(&self, normalized_word: &str) -> Option<(String, u16)> {
        if let Some(pending_update) = self.pending_updates.get(normalized_word) {
            if let PendingWordListUpdate::AddOrUpdate { canonical, score } = pending_update {
                Some((canonical.clone(), *score))
            } else {
                None
            }
        } else if let Some(entry_index) = self.index.get(normalized_word) {
            let entry = &self.entries[*entry_index];
            Some((entry.canonical.clone(), entry.score))
        } else {
            None
        }
    }
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
    pub score: u16,
}

fn parse_word_list_file_contents(
    file_contents: &str,
    index: &mut HashMap<String, usize>,
    errors: &mut Vec<WordListError>,
) -> Vec<RawWordListEntry> {
    let mut entries = Vec::with_capacity(file_contents.lines().count());

    for line in file_contents.lines() {
        if errors.len() > 100 {
            break;
        }

        let line_parts: Vec<_> = line.split(';').collect();

        if line_parts[0].chars().any(|c| c == '�') {
            errors.push(WordListError::InvalidWord(line_parts[0].into()));
            continue;
        }

        let canonical = line_parts[0].trim().to_string();
        let normalized = normalize_word(&canonical);
        if normalized.is_empty() {
            continue;
        }
        if index.contains_key(&normalized) {
            continue;
        }

        let Ok(score) = (if line_parts.len() < 2 {
            Ok(50)
        } else {
            line_parts[1].trim().parse::<u16>()
        }) else {
            errors.push(WordListError::InvalidScore(line_parts[1].into()));
            continue;
        };

        index.insert(normalized.clone(), entries.len());
        entries.push(RawWordListEntry {
            length: normalized.chars().count(),
            normalized,
            canonical,
            score,
        });
    }

    entries
}

fn read_file_tolerating_invalid_encoding(path: &OsString) -> Result<String, io::Error> {
    let mut file = File::open(path)?;
    let mut buf = vec![];
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into())
}

pub struct RawWordListContents {
    pub entries: Vec<RawWordListEntry>,
    pub mtime: Option<SystemTime>,
    pub index: HashMap<String, usize>,
    pub errors: Vec<WordListError>,
}

#[must_use]
pub fn load_words_from_source(source: &WordListSourceConfig) -> RawWordListContents {
    let mtime = source.modified();
    let mut index = HashMap::new();
    let mut errors = vec![];

    let entries = match source {
        WordListSourceConfig::Memory { words, .. } => {
            let mut entries = Vec::with_capacity(words.len());

            for (canonical, score) in words.iter().cloned() {
                let normalized = normalize_word(&canonical);
                if normalized.is_empty() {
                    continue;
                }
                if index.contains_key(&normalized) {
                    continue;
                }

                index.insert(normalized.clone(), entries.len());
                entries.push(RawWordListEntry {
                    length: normalized.chars().count(),
                    normalized,
                    canonical,
                    score,
                });
            }

            entries
        }

        WordListSourceConfig::File { path, .. } => {
            if let Ok(contents) = read_file_tolerating_invalid_encoding(path) {
                parse_word_list_file_contents(&contents, &mut index, &mut errors)
            } else {
                errors.push(WordListError::InvalidPath(path.to_string_lossy().into()));
                vec![]
            }
        }

        WordListSourceConfig::FileContents { contents, .. } => {
            parse_word_list_file_contents(contents, &mut index, &mut errors)
        }
    };

    RawWordListContents {
        entries,
        mtime,
        index,
        errors,
    }
}

/// Refresh the given source, regardless of whether it appears to need it.
pub fn refresh_source(
    source: &WordListSourceConfig,
    source_index: u16,
    source_states: &mut HashMap<String, WordListSourceState>,
) {
    let RawWordListContents {
        entries,
        mtime,
        index,
        errors,
    } = load_words_from_source(source);

    let mut new_state = WordListSourceState {
        source_index,
        id: source.id(),
        entries,
        mtime,
        index,
        errors,
        pending_updates: HashMap::new(),
    };

    let old_state = source_states.remove(&new_state.id);
    if let Some(old_state) = old_state {
        new_state.pending_updates = old_state.pending_updates;
    }

    source_states.insert(new_state.id.clone(), new_state);
}

/// Refresh the given source unless we know it hasn't been modified.
pub fn refresh_source_if_needed(
    source: &WordListSourceConfig,
    source_index: u16,
    source_states: &mut HashMap<String, WordListSourceState>,
) {
    let old_state = source_states.get_mut(&source.id());
    if let Some(old_state) = old_state {
        let new_mtime = source.modified();
        if new_mtime.is_some() && new_mtime == old_state.mtime {
            old_state.source_index = source_index;
            return;
        }
    }

    refresh_source(source, source_index, source_states);
}

type OnUpdateCallback = Box<dyn FnMut(&mut WordList, &[GlobalWordId]) + Send + Sync>;

/// Errors that can arise when syncing to disk, keyed by the relevant source id.
pub type SyncErrors = HashMap<String, io::Error>;

/// A struct representing the currently-loaded word list(s). This contains information that is
/// static regardless of grid geometry or our progress through a fill (although we do configure a
/// `max_length` that depends on the size of the grid, since it helps performance to avoid
/// loading words that are too long to be usable). The word list can be modified without disrupting
/// the indices of any glyphs or words.
#[allow(dead_code)]
pub struct WordList {
    /// A list of all characters that occur in any (normalized) word. `GlyphId`s used everywhere
    /// else are indices into this list.
    pub glyphs: Vec<char>,

    /// The inverse of `glyphs`: a map from a character to the `GlyphId` representing it.
    pub glyph_id_by_char: HashMap<char, GlyphId>,

    /// A list of all loaded words, bucketed by length. An index into `words` is the length of the
    /// words in the bucket, so `words[0]` is always an empty vec.
    pub words: Vec<Vec<Word>>,

    /// A map from a normalized string to the id of the Word representing it.
    pub word_id_by_string: HashMap<String, WordId>,

    /// A dupe index reflecting the max substring length provided when configuring the `WordList`.
    pub dupe_index: BoxedDupeIndex,

    /// The maximum word length provided when configuring the `WordList`, if any.
    pub max_length: Option<usize>,

    /// Callback run after adding words.
    pub on_update: Option<OnUpdateCallback>,

    /// The most recently-received word list sources, as an ordered list.
    pub source_configs: Vec<WordListSourceConfig>,

    /// If applicable, the index of the source that should be treated as the personal list.
    pub personal_list_index: Option<u16>,

    /// The last seen state of each word list source, keyed by source id.
    pub source_states: HashMap<String, WordListSourceState>,

    /// Do we have pending updates that need to be saved? If we try to sync and it fails, we'll
    /// reset this to `false` to avoid infinite-looping, but the changes will still be available on
    /// the individual sources and will be retried next time we attempt a sync.
    pub needs_sync: bool,
}

impl WordList {
    /// Construct a new `WordList` using the given sources (omitting any entries that are longer
    /// than `max_length`).
    #[allow(dead_code)]
    #[must_use]
    pub fn new(
        source_configs: Vec<WordListSourceConfig>,
        personal_list_index: Option<u16>,
        max_length: Option<usize>,
        max_shared_substring: Option<usize>,
    ) -> WordList {
        let mut instance = WordList {
            glyphs: vec![],
            glyph_id_by_char: HashMap::new(),
            words: vec![vec![]],
            word_id_by_string: HashMap::new(),
            dupe_index: WordList::instantiate_dupe_index(max_shared_substring),
            max_length,
            on_update: None,
            source_configs: vec![],
            personal_list_index,
            source_states: HashMap::new(),
            needs_sync: false,
        };

        instance.replace_list(source_configs, personal_list_index, max_length, false);

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

    /// Add the given word to the list as a hidden entry and trigger the update callback. The word
    /// must not be part of the list yet.
    fn add_hidden_word(&mut self, normalized_word: &str) -> GlobalWordId {
        let global_word_id = self.add_word_silent(
            &RawWordListEntry {
                length: normalized_word.chars().count(),
                normalized: normalized_word.to_string(),
                canonical: normalized_word.to_string(),
                score: 0,
            },
            None,
            true,
        );

        if let Some(mut on_update) = self.on_update.take() {
            on_update(self, &[global_word_id]);
            self.on_update = Some(on_update);
        }

        global_word_id
    }

    /// Add the given word to the list without triggering the update callback. The word must not be
    /// part of the list yet.
    fn add_word_silent(
        &mut self,
        raw_entry: &RawWordListEntry,
        source_index: Option<u16>,
        hidden: bool,
    ) -> GlobalWordId {
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
            score: raw_entry.score,
            letter_score: raw_entry
                .normalized
                .chars()
                .map(|char| LETTER_POINTS.get(&char).copied().unwrap_or(3))
                .sum(),
            hidden,
            source_index,
            personal_word_score: if self
                .personal_list_index
                .map_or(false, |idx| Some(idx) == source_index)
            {
                Some(raw_entry.score)
            } else {
                None
            },
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
    ///   hidden or have reduced their score. This is for updating external caches of fill data
    ///   that need to know whether we've added or removed any possible fills.
    ///
    /// - If `silent` is false and an `on_update` callback is present, we track
    ///   any words that are added to the list (independent of hidden status and score) and
    ///   pass them into the callback. This is for updating things like dupe indexes that need
    ///   a comprehensive picture of all tracked words, regardless of whether they're available for
    ///   fill purposes.
    pub fn replace_list(
        &mut self,
        source_configs: Vec<WordListSourceConfig>,
        personal_list_index: Option<u16>,
        max_length: Option<usize>,
        silent: bool,
    ) -> (bool, HashSet<GlobalWordId>) {
        self.source_configs = source_configs;
        self.personal_list_index = personal_list_index;
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

        let mut hidden_personal_scores: HashMap<GlobalWordId, u16> = HashMap::new();

        self.load_words_from_source_configs(
            max_length,
            |word_list, raw_entry, source_index| {
                let word_length = raw_entry.length;
                let existing_word_id = word_list.word_id_by_string.get(&raw_entry.normalized);

                if let Some(&existing_word_id) = existing_word_id {
                    let word = &mut word_list.words[word_length][existing_word_id];
                    if word.hidden || raw_entry.score > word.score {
                        any_more_visible = true;
                    }
                    if !word.hidden && raw_entry.score < word.score {
                        less_visible_words_set.insert((word_length, existing_word_id));
                    }
                    word.score = raw_entry.score;
                    word.hidden = false;
                    word.canonical_string.clone_from(&raw_entry.canonical);
                    word.source_index = Some(source_index);
                    word.personal_word_score =
                        if personal_list_index.map_or(false, |idx| idx == source_index) {
                            Some(raw_entry.score)
                        } else {
                            None
                        };
                    removed_words_set.remove(&(word_length, existing_word_id));
                } else if !silent {
                    any_more_visible = true;
                    let added_word_id =
                        word_list.add_word_silent(raw_entry, Some(source_index), false);
                    newly_added_words.push(added_word_id);
                } else {
                    any_more_visible = true;
                    word_list.add_word_silent(raw_entry, Some(source_index), false);
                }
            },
            |word_list, raw_entry| {
                let word_id = word_list.get_word_id_or_add_hidden(&raw_entry.normalized);
                hidden_personal_scores.insert(word_id, raw_entry.score);
            },
        );

        // Hide any words that were in our existing list but aren't in the new one.
        for &(length, word_id) in &removed_words_set {
            self.words[length][word_id].hidden = true;
            self.words[length][word_id].source_index = None;
            self.words[length][word_id].personal_word_score = None;
            less_visible_words_set.insert((length, word_id));
        }

        // Finally, if the personal list contained any words shadowed by higher-priority lists,
        // or the whole personal list is disabled, set the `personal_word_score` field on words
        // that weren't already updated.
        for ((length, word_id), score) in hidden_personal_scores {
            self.words[length][word_id].personal_word_score = Some(score);
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
        mut add_word: impl FnMut(&mut WordList, &RawWordListEntry, u16),
        mut handle_disabled_personal_entry: impl FnMut(&mut WordList, &RawWordListEntry),
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

        for (source_index, source) in source_configs.iter().enumerate() {
            let is_source_enabled = source.enabled();
            let is_personal_list = self
                .personal_list_index
                .map_or(false, |idx| idx == (source_index as u16));

            refresh_source_if_needed(source, source_index as u16, &mut source_states);

            // If the source is disabled, none of its words (or pending updates) should affect the
            // actual wordlist. The exception is if this is the personal word list, in which case
            // we still need to process it to populate the `personal_word_score` fields.
            if !is_source_enabled && !is_personal_list {
                continue;
            }

            let source_state = source_states
                .get(&source.id())
                .expect("source state must be defined after refreshing");

            // If there are any pending updates, they take priority over the list items as stored.
            let mut updated_words: Vec<RawWordListEntry> = vec![];
            let superseded_words = if source_state.pending_updates.is_empty() {
                None
            } else {
                let mut superseded_words = HashSet::new();
                for (normalized, pending_update) in &source_state.pending_updates {
                    superseded_words.insert(normalized.clone());

                    if let PendingWordListUpdate::AddOrUpdate { canonical, score } = pending_update
                    {
                        updated_words.push(RawWordListEntry {
                            length: normalized.chars().count(),
                            normalized: normalized.clone(),
                            canonical: canonical.clone(),
                            score: *score,
                        });
                    }
                }
                Some(superseded_words)
            };

            let mut process_word = |word: &RawWordListEntry| {
                if let Some(max_length) = max_length {
                    if word.length > max_length {
                        return;
                    }
                }
                if !is_source_enabled && is_personal_list {
                    handle_disabled_personal_entry(self, word);
                    return;
                }
                let hash = hash_str(&word.normalized);
                if seen_words.contains(&hash) {
                    if is_personal_list {
                        handle_disabled_personal_entry(self, word);
                    }
                    return;
                }
                add_word(self, word, source_state.source_index);
                seen_words.insert(hash);
            };
            for word in &updated_words {
                process_word(word);
            }
            for word in &source_state.entries {
                if let Some(superseded_words) = superseded_words.as_ref() {
                    if superseded_words.contains(&word.normalized) {
                        continue;
                    }
                }
                process_word(word);
            }
        }

        self.source_configs = source_configs;
        self.source_states = source_states;
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

    /// Update the word list state to be consistent with the given word being upserted into the
    /// given source. Return the source's previous entry for that normalized word, if
    /// applicable.
    pub fn optimistically_update_word(
        &mut self,
        canonical: &str,
        score: u16,
        source_id: &str,
    ) -> Option<(String, u16)> {
        let normalized = normalize_word(canonical);
        if normalized.is_empty() {
            return None;
        }

        let source_index = self.find_source_index_for_id(source_id)?;
        let source_config = &self.source_configs[source_index as usize];
        let is_personal_list = self
            .personal_list_index
            .map_or(false, |idx| idx == source_index);

        let source_id = source_config.id();
        let Some(source_state) = self.source_states.get_mut(&source_id) else {
            panic!("optimistically_update_word: no source state found for id");
        };

        let previous_entry = source_state.get_entry(&normalized);

        // Regardless of whether this change is visible in `words`, we need to buffer it
        // to be persisted to the file.
        source_state.pending_updates.insert(
            normalized.clone(),
            PendingWordListUpdate::AddOrUpdate {
                canonical: canonical.into(),
                score,
            },
        );
        self.needs_sync = true;

        // If the source is disabled, we don't need to update the actual wordlist, except to set
        // `personal_word_score` if applicable.
        if !source_config.enabled() {
            if is_personal_list {
                let (length, word_id) = self.get_word_id_or_add_hidden(&normalized);
                self.words[length][word_id].personal_word_score = Some(score);
            }
            return previous_entry;
        }

        let (length, word_id) = self.get_word_id_or_add_hidden(&normalized);
        let word = &mut self.words[length][word_id];

        if is_personal_list {
            word.personal_word_score = Some(score);
        }

        let should_update = word
            .source_index
            .map_or(true, |existing_index| source_index <= existing_index);

        if !should_update {
            return previous_entry;
        }

        word.canonical_string = canonical.into();
        word.score = score;
        word.hidden = false;
        word.source_index = Some(source_index);
        previous_entry
    }

    /// Update the word list state to be consistent with the given word being deleted from
    /// the given source. Return the source's previous entry for that normalized word, if
    /// applicable.
    pub fn optimistically_delete_word(
        &mut self,
        normalized: &str,
        source_id: &str,
    ) -> Option<(String, u16)> {
        let source_index = self.find_source_index_for_id(source_id)?;
        let source_config = &self.source_configs[source_index as usize];
        let is_personal_list = self
            .personal_list_index
            .map_or(false, |idx| idx == source_index);

        // Regardless of whether this change is visible in `words`, we need to buffer it
        // to be persisted to the file.
        let source_id = source_config.id();
        let Some(source_state) = self.source_states.get_mut(&source_id) else {
            panic!("optimistically_delete_word: no source state found for id");
        };

        let previous_entry = source_state.get_entry(normalized);

        source_state
            .pending_updates
            .insert(normalized.to_string(), PendingWordListUpdate::Delete);
        self.needs_sync = true;

        // If the source is disabled, we don't need to update the actual wordlist, except to clear
        // `personal_word_score` if applicable.
        if !source_config.enabled() {
            if is_personal_list {
                if let Some(word_id) = self.word_id_by_string.get(normalized) {
                    self.words[normalized.chars().count()][*word_id].personal_word_score = None;
                }
            }
            return previous_entry;
        }

        let length = normalized.chars().count();
        let Some(&word_id) = self.word_id_by_string.get(normalized) else {
            #[cfg(feature = "check_invariants")]
            panic!("optimistically_delete_word_impl: word doesn't exist");
            #[cfg(not(feature = "check_invariants"))]
            return previous_entry;
        };

        let word = &mut self.words[length][word_id];

        // Whatever else happens, we should clear the personal score iff we're deleting from the
        // personal list. Otherwise it should stay as is.
        if is_personal_list {
            word.personal_word_score = None;
        }

        // If the version we have stored isn't from this list, no action is needed --
        // either it's not present in the list at all, or it's already shadowed by a
        // higher-priority source.
        if word.source_index != Some(source_index) {
            return previous_entry;
        }

        // Otherwise, we need to look through any lower-ranked, enabled sources in order to see if
        // any of them contain the word.
        for other_source_index in (source_index as usize + 1)..(self.source_configs.len()) {
            let other_source = &self.source_configs[other_source_index];
            if !other_source.enabled() {
                continue;
            }
            let Some(other_source_state) = self.source_states.get(&other_source.id()) else {
                continue;
            };
            let Some(&entry_index) = other_source_state.index.get(&word.normalized_string) else {
                continue;
            };
            let entry = &other_source_state.entries[entry_index];
            word.score = entry.score;
            word.canonical_string.clone_from(&entry.canonical);
            word.hidden = false;
            word.source_index = Some(other_source_index as u16);
            return previous_entry;
        }

        // If they didn't, we can safely hide it.
        word.hidden = true;
        word.source_index = None;
        previous_entry
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

    /// If there are any pending updates, write them to disk and refresh the contents of the
    /// affected list(s). Return a flag indicating whether we need to refresh the overall word
    /// list; this is true only if we think the end state of the individual lists might be
    /// inconsistent with the previous state counting pending updates. If any pending updates can't
    /// be written (probably due to something like permissions issues or a drive not being
    /// mounted), return error info, reset `sync_state` to `Synced`, and keep the pending updates
    /// in place.
    pub fn sync_updates_to_disk(&mut self) -> (bool, SyncErrors) {
        let mut should_refresh_overall = false;
        let mut sync_errors: SyncErrors = HashMap::new();

        let source_ids: Vec<_> = self.source_states.keys().cloned().collect();

        // For each source file, write any pending updates.
        for source_id in source_ids {
            let source_state = self.source_states.get_mut(&source_id).unwrap();

            if source_state.pending_updates.is_empty() {
                continue;
            }

            let source_config = &self.source_configs[source_state.source_index as usize];

            // If the file was updated externally since the last time we reloaded it, there's no
            // problem with this process since we'd read it from disk either way, but unless it's
            // disabled we'll definitely want to do a full refresh of the word list when we're done
            // syncing.
            if source_config.enabled() && source_state.mtime != source_config.modified() {
                should_refresh_overall = true;
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
            let file_contents = match read_file_tolerating_invalid_encoding(&path) {
                Ok(contents) => contents,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    should_refresh_overall = true;
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

                    if !pending_updates.contains_key(&normalized) {
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

            let mut new_file_contents = String::with_capacity(file_contents.capacity());
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

            // Now that we've updated the file, clear the pending updates and refresh its contents
            // from disk.
            source_state.pending_updates = HashMap::new();
            let source_index = source_state.source_index;
            refresh_source(source_config, source_index, &mut self.source_states);
        }

        // Regardless of whether we actually succeeded in syncing everything, we should
        // reset this flag so that we don't end up accidentally infinite-looping. Calling
        // `sync_updates_to_disk` again will still retry and files that failed, since the
        // pending updates are still present.
        self.needs_sync = false;

        (should_refresh_overall, sync_errors)
    }

    /// Reload the word list(s) using the current config. Returns the same information as
    /// `replace_list`.
    pub fn refresh_from_disk(&mut self) -> (bool, HashSet<GlobalWordId>) {
        self.replace_list(
            self.source_configs.clone(),
            self.personal_list_index,
            self.max_length,
            false,
        )
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

                if old_mtime == new_mtime {
                    None
                } else {
                    Some(id)
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
    use crate::word_list::{WordList, WordListSourceConfig};
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
        let word_list = WordList::new(word_list_source_config(), None, Some(5), None);

        assert_eq!(word_list.max_length, Some(5));
        assert_eq!(word_list.words.len(), 6);

        let &word_id = word_list
            .word_id_by_string
            .get("skate")
            .expect("word list should include 'skate'");

        let word = &word_list.words[5][word_id];
        assert_eq!(word.normalized_string, "skate");
        assert_eq!(word.canonical_string, "skate");
        assert_eq!(word.score, 50);
        assert_eq!(word.hidden, false);

        assert!(!word_list.word_id_by_string.contains_key("skates"));
    }

    #[test]
    fn test_dynamic_max_length() {
        let mut word_list = WordList::new(word_list_source_config(), None, None, None);

        assert_eq!(word_list.max_length, None);
        assert_eq!(word_list.words.len(), 16);

        let &word_id = word_list
            .word_id_by_string
            .get("skates")
            .expect("word list should include 'skates'");

        let word = &word_list.words[6][word_id];
        assert_eq!(word.hidden, false);
        assert_eq!(word.source_index, Some(0));

        word_list.replace_list(word_list_source_config(), None, Some(5), false);

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
            None,
        );

        assert_eq!(
            word_list.words.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 1, 0, 1]
        );
    }

    #[test]
    fn test_soft_dupe_index() {
        let mut word_list = WordList::new(vec![], None, Some(6), Some(5));
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
            None,
        );
        assert!(word_list.get_source_errors().get("0").unwrap().is_empty());
        assert!(word_list.get_source_errors().get("1").unwrap().is_empty());

        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let wolvvves_id = word_list.get_word_id_or_add_hidden("wolvvves");
        let wharves_id = word_list.get_word_id_or_add_hidden("wharves");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 70);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(0));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(1));
        }

        assert_eq!(
            word_list.optimistically_update_word("wolves", 72, "0"),
            Some(("wolves".into(), 70))
        );
        assert_eq!(
            word_list.optimistically_update_word("wharves", 73, "0"),
            None,
        );
        assert_eq!(word_list.optimistically_update_word("worfs", 74, "0"), None);

        let worfs_id = word_list.get_word_id_or_add_hidden("worfs");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 72);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 73);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 74);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(0));
        }

        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 72))
        );
        assert_eq!(
            word_list.optimistically_delete_word("worfs", "0"),
            Some(("worfs".into(), 74))
        );

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.hidden, true);
            assert_eq!(worfs.source_index, None);
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
            None,
            false,
        );

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 71);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.hidden, true);
            assert_eq!(worfs.source_index, None);
        }

        assert_eq!(
            word_list.optimistically_update_word("wolves", 80, "0"),
            None // delete was still pending from before
        );
        assert_eq!(
            word_list.optimistically_update_word("wolvvves", 81, "0"),
            Some(("wolvvves".into(), 71))
        );
        assert_eq!(
            word_list.optimistically_update_word("wharves", 82, "0"),
            Some(("wharves".into(), 73)) // add was still pending from before
        );
        assert_eq!(word_list.optimistically_update_word("worfs", 83, "0"), None);

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let wolvvves = word_list.get_word(wolvvves_id);
            assert_eq!(wolvvves.score, 81);
            assert_eq!(wolvvves.hidden, false);
            assert_eq!(wolvvves.source_index, Some(1));
        }
        {
            let wharves = word_list.get_word(wharves_id);
            assert_eq!(wharves.score, 50);
            assert_eq!(wharves.hidden, false);
            assert_eq!(wharves.source_index, Some(0));
        }
        {
            let worfs = word_list.get_word(worfs_id);
            assert_eq!(worfs.score, 83);
            assert_eq!(worfs.hidden, false);
            assert_eq!(worfs.source_index, Some(1));
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
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");
        let what_id = word_list.get_word_id_or_add_hidden("what");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 51);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 54);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
        }
        {
            let what = word_list.get_word(what_id);
            assert_eq!(what.score, 50);
            assert_eq!(what.hidden, false);
            assert_eq!(what.source_index, Some(1));
        }

        assert_eq!(
            word_list.optimistically_update_word("wOl vEs", 60, "0"),
            Some(("wolves".into(), 51))
        );
        assert_eq!(
            word_list.optimistically_delete_word("steev", "0"),
            Some(("steev".into(), 54))
        );
        assert_eq!(
            word_list.optimistically_update_word("Wolves", 99, "2"),
            Some(("wolves".into(), 52))
        );
        assert_eq!(
            word_list.optimistically_update_word("tonberry", 30, "2"),
            None
        );
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wOl vEs");
            assert_eq!(wolves.score, 60);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 54);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
        }
        {
            let tonberry_id = word_list.get_word_id_or_add_hidden("tonberry");
            let tonberry = word_list.get_word(tonberry_id);
            assert_eq!(tonberry.score, 30);
            assert_eq!(tonberry.hidden, false);
            assert_eq!(tonberry.source_index, Some(2));
        }
        assert!(word_list.needs_sync);

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
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");

        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 51);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(0));
        }

        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 51))
        );
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        assert!(word_list.needs_sync);

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(!should_refresh);
        assert!(sync_errors.is_empty());

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
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
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // See previous test
        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 51))
        );
        assert_eq!(
            word_list.optimistically_update_word("Steev", 55, "0"),
            Some(("steev".into(), 54))
        );
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.canonical_string, "Steev");
            assert_eq!(steev.score, 55);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
        }
        assert!(word_list.needs_sync);

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
            None,
            false,
        );
        assert!(any_more_visible);
        assert_eq!(
            less_visible_words,
            HashSet::from([word_list.get_word_id_or_add_hidden("ijsnzlsj")])
        );
        assert!(word_list.needs_sync);
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.canonical_string, "wolves");
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.canonical_string, "Steev");
            assert_eq!(steev.score, 55);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
        }

        // Make another out-of-band change, so we can validate that it gets loaded again
        // when we sync.
        fs::write(tmpfile.path(), "wolves;53\nsteev;47\nabcdedcba;22").unwrap();

        // Now we can sync, merging the in-memory changes with our out-of-band changes.
        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert!(sync_errors.is_empty());
        assert!(!word_list.needs_sync);

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
            None,
        );
        let idkidkidk_id = word_list.get_word_id_or_add_hidden("idkidkidk");

        // See previous tests
        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 51))
        );
        assert_eq!(
            word_list.optimistically_update_word("Steev", 55, "0"),
            Some(("steev".into(), 54))
        );
        assert!(word_list.needs_sync);

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
        assert!(!word_list.needs_sync);

        let (any_more_visible, less_visible_words) = word_list.refresh_from_disk();
        assert!(!any_more_visible);
        assert_eq!(less_visible_words, HashSet::from([idkidkidk_id]));

        // Now, if we remove the directory, we should be able to write the updates to a
        // new file in the same location. Note that `idkidkidk` is gone though, since we
        // always prioritize the current state of the file over what we loaded before, and
        // now it's empty.
        fs::remove_dir(tmpfile.path()).unwrap();

        let (should_refresh, sync_errors) = word_list.sync_updates_to_disk();
        assert!(should_refresh);
        assert!(sync_errors.is_empty());
        assert!(!word_list.needs_sync);

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
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // The disabled list doesn't affect the actual word list.
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
        }

        // Optimistic updates elevate the sync state but still don't affect the actual word list.
        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 51))
        );
        assert_eq!(
            word_list.optimistically_update_word("S t eev", 99, "0"),
            Some(("steev".into(), 54))
        );
        assert_eq!(
            word_list.optimistically_update_word("sT eev", 51, "0"),
            Some(("S t eev".into(), 99))
        );
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.hidden, true);
            assert_eq!(steev.source_index, None);
        }
        assert!(word_list.needs_sync);

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
            None,
        );
        let wolves_id = word_list.get_word_id_or_add_hidden("wolves");
        let steev_id = word_list.get_word_id_or_add_hidden("steev");

        // This is like the last one, except that between updating the list and syncing it we
        // re-enable it. This applies the buffered changes to the resulting word list.
        assert_eq!(
            word_list.optimistically_delete_word("wolves", "0"),
            Some(("wolves".into(), 51))
        );
        assert_eq!(
            word_list.optimistically_update_word("S t eev", 99, "0"),
            Some(("steev".into(), 54))
        );
        assert_eq!(
            word_list.optimistically_update_word("sT eev", 51, "0"),
            Some(("S t eev".into(), 99))
        );
        assert!(word_list.needs_sync);

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
            None,
            false,
        );
        assert!(any_more_visible); // steev
        assert!(less_visible_words.is_empty());
        {
            let wolves = word_list.get_word(wolves_id);
            assert_eq!(wolves.score, 50);
            assert_eq!(wolves.hidden, false);
            assert_eq!(wolves.source_index, Some(1));
        }
        {
            let steev = word_list.get_word(steev_id);
            assert_eq!(steev.score, 51);
            assert_eq!(steev.hidden, false);
            assert_eq!(steev.source_index, Some(0));
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
