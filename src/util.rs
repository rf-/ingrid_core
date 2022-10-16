use smallvec::SmallVec;

use crate::word_list::{WordId, WordList};
use crate::MAX_GLYPH_COUNT;

/// Structure tracking number of occurrences in a slot's options of each glyph in each cell.
/// TODO: fix doc
pub type GlyphCountsByCell = Vec<SmallVec<[u32; MAX_GLYPH_COUNT]>>;

/// Initialize the `glyph_counts_by_cell` structure for a slot.
/// TODO: fix doc
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
