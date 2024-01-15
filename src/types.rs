/// An identifier for a given letter or symbol, based on its index in the `WordList`'s `glyphs`
/// field.
pub type GlyphId = usize;

/// An identifier for a given word, based on its index in the `WordList`'s `words` field (scoped to
/// the relevant length bucket).
pub type WordId = usize;

/// An identifier that fully specifies a word by including both its length and `WordId`.
pub type GlobalWordId = (usize, WordId);
