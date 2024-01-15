use smallvec::SmallVec;

use crate::types::WordId;
use crate::word_list::WordList;
use crate::MAX_GLYPH_COUNT;

/// Structure used to efficiently prune options based on their crossings. One of these reflects all
/// of the currently-available options for a single slot.
///
/// The outer `Vec` has an entry for each cell; each of these entries consists of a `SmallVec`
/// indexed by `GlyphId`, containing the number of times that glyph occurs in that position in
/// all of the available options.
///
pub type GlyphCountsByCell = Vec<SmallVec<[u32; MAX_GLYPH_COUNT]>>;

/// Initialize the `glyph_counts_by_cell` structure for a slot.
#[must_use]
pub fn build_glyph_counts_by_cell(
    word_list: &WordList,
    slot_length: usize,
    options: &[WordId],
) -> GlyphCountsByCell {
    let mut result: GlyphCountsByCell = (0..slot_length)
        .map(|_| (0..word_list.glyphs.len()).map(|_| 0).collect())
        .collect();

    for &word_id in options {
        let word = &word_list.words[slot_length][word_id];
        for (cell_idx, &glyph) in word.glyphs.iter().enumerate() {
            result[cell_idx][glyph] += 1;
        }
    }

    result
}
