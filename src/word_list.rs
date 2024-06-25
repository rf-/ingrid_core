use lazy_static::lazy_static;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::{fmt, io, mem};
use unicode_normalization::UnicodeNormalization;

use crate::types::{GlobalWordId, GlyphId, WordId};
use crate::{MAX_GLYPH_COUNT, MAX_SLOT_LENGTH};

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

    /// The glyph ids making up `normalized_string`.
    pub glyphs: SmallVec<[GlyphId; MAX_SLOT_LENGTH]>,

    /// The word's score, usually on a roughly 0 - 100 scale where 50 means average quality.
    pub score: u16,

    /// The sum of the scores of the word's letters.
    pub letter_score: u16,
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
        words: Vec<(String, u16)>,
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
}

pub struct WordListSourceState {
    pub entries: Vec<RawWordListEntry>,
    pub errors: Vec<WordListError>,
}

/// `WordListSourceState`s keyed by the `id` of the relevant source.
pub type WordListSourceStates = HashMap<String, WordListSourceState>;

/// A single word list entry.
#[allow(dead_code)]
#[derive(Debug)]
pub struct RawWordListEntry {
    pub length: usize,
    pub normalized: String,
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
            score,
        });
    }

    entries
}

fn read_file_tolerating_invalid_encoding(path: &OsString) -> Result<String, io::Error> {
    let mut file = File::open(path)?;
    let mut buf = vec![];
    file.read_to_end(&mut buf)?;
    return Ok(String::from_utf8_lossy(&buf).into());
}

pub struct RawWordListContents {
    pub entries: Vec<RawWordListEntry>,
    pub errors: Vec<WordListError>,
}

#[must_use]
pub fn load_words_from_source(source: &WordListSourceConfig) -> RawWordListContents {
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

    RawWordListContents { entries, errors }
}

pub fn load_source(
    source: &WordListSourceConfig,
    source_states: &mut HashMap<String, WordListSourceState>,
) {
    let RawWordListContents { entries, errors } = load_words_from_source(source);

    source_states.insert(source.id(), WordListSourceState { entries, errors });
}

/// A struct representing the currently-loaded word list(s). This contains information that is
/// static regardless of grid geometry or our progress through a fill (although we do configure a
/// `max_length` that depends on the size of the grid, since it helps performance to avoid
/// loading words that are too long to be usable).
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

    /// The maximum word length provided when configuring the WordList, if any.
    pub max_length: Option<usize>,

    /// The last seen state of each word list source, keyed by source id.
    pub source_states: HashMap<String, WordListSourceState>,
}

impl WordList {
    /// Construct a new `WordList` using the given sources (omitting any entries that are longer
    /// than `max_length`).
    #[allow(dead_code)]
    #[must_use]
    pub fn new(source_configs: &[WordListSourceConfig], max_length: Option<usize>) -> WordList {
        let mut instance = WordList {
            glyphs: smallvec![],
            glyph_id_by_char: HashMap::new(),
            words: vec![vec![]],
            word_id_by_string: HashMap::new(),
            max_length,
            source_states: HashMap::new(),
        };

        instance.populate_list(source_configs);

        instance
    }

    /// If the given normalized word is already in the list, return its id; if not, add it.
    pub fn get_word_id_or_add_word(&mut self, normalized_word: &str) -> GlobalWordId {
        self.word_id_by_string
            .get(normalized_word)
            .copied()
            .map_or_else(
                || {
                    self.add_word(&RawWordListEntry {
                        length: normalized_word.chars().count(),
                        normalized: normalized_word.into(),
                        score: 0,
                    })
                },
                |word_id| (normalized_word.chars().count(), word_id),
            )
    }

    /// Borrow an existing word using its global id.
    #[must_use]
    pub fn get_word(&self, global_word_id: GlobalWordId) -> &Word {
        &self.words[global_word_id.0][global_word_id.1]
    }

    /// Add the given word to the list. The word must not be part of the list yet.
    fn add_word(&mut self, raw_entry: &RawWordListEntry) -> GlobalWordId {
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
            glyphs,
            score: raw_entry.score,
            letter_score: raw_entry
                .normalized
                .chars()
                .map(|char| LETTER_POINTS.get(&char).copied().unwrap_or(3))
                .sum(),
        });

        self.word_id_by_string
            .insert(raw_entry.normalized.clone(), word_id);

        (word_length, word_id)
    }

    pub fn populate_list(&mut self, source_configs: &[WordListSourceConfig]) {
        fn hash_str(str: &str) -> u64 {
            let mut hasher = DefaultHasher::new();
            str.hash(&mut hasher);
            hasher.finish()
        }

        let mut source_states = mem::take(&mut self.source_states);
        let mut seen_words: HashSet<u64> = HashSet::new();

        for source in source_configs {
            load_source(source, &mut source_states);

            let source_state = source_states
                .get(&source.id())
                .expect("source state must be defined after refreshing");

            for raw_word in &source_state.entries {
                if let Some(max_length) = self.max_length {
                    if raw_word.length > max_length {
                        continue;
                    }
                }
                let hash = hash_str(&raw_word.normalized);
                if seen_words.contains(&hash) {
                    continue;
                }

                let word_length = raw_word.length;
                let existing_word_id = self.word_id_by_string.get(&raw_word.normalized);

                if let Some(&existing_word_id) = existing_word_id {
                    let word = &mut self.words[word_length][existing_word_id];
                    word.score = raw_word.score;
                } else {
                    self.add_word(raw_word);
                }
                seen_words.insert(hash);
            }
        }

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

    /// For each source provided last time we loaded or updated, return any errors it emitted.
    #[must_use]
    pub fn get_source_errors(&self) -> HashMap<String, Vec<WordListError>> {
        let mut source_errors = HashMap::new();

        for (source_id, source_state) in &self.source_states {
            source_errors.insert(source_id.clone(), source_state.errors.clone());
        }

        source_errors
    }
}

#[cfg(test)]
#[allow(clippy::bool_assert_comparison)]
#[allow(clippy::float_cmp)]
#[allow(clippy::too_many_lines)]
#[allow(clippy::similar_names)]
pub mod tests {
    use crate::word_list::{WordList, WordListSourceConfig};
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

    #[must_use]
    pub fn word_list_source_config() -> Vec<WordListSourceConfig> {
        vec![WordListSourceConfig::File {
            id: "0".into(),
            path: dictionary_path().into(),
        }]
    }

    #[test]
    fn test_loads_words_up_to_max_length() {
        let word_list = WordList::new(&word_list_source_config(), Some(5));

        assert_eq!(word_list.max_length, Some(5));
        assert_eq!(word_list.words.len(), 6);

        let &word_id = word_list
            .word_id_by_string
            .get("skate")
            .expect("word list should include 'skate'");

        let word = &word_list.words[5][word_id];
        assert_eq!(word.normalized_string, "skate");
        assert_eq!(word.score, 50);

        assert!(word_list.word_id_by_string.get("skates").is_none());
    }

    #[test]
    #[allow(clippy::unicode_not_nfc)]
    fn test_unusual_characters() {
        let word_list = WordList::new(
            &[WordListSourceConfig::Memory {
                id: "0".into(),
                words: vec![
                    // Non-English character expressed as one two-byte `char`
                    ("monsutâ".into(), 50),
                    // Non-English character expressed as two chars w/ combining form
                    ("hélen".into(), 50),
                ],
            }],
            None,
        );

        assert_eq!(
            word_list.words.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 1, 0, 1]
        );
    }
}
