pub mod arc_consistency;
pub mod backtracking_search;
pub mod grid_config;
pub mod util;
pub mod word_list;

/// Should we run extra checks to validate that we're never in an invalid state during search? This
/// can be enabled with `--features check_invariants` when debugging or making risky algorithm
/// changes.
pub const CHECK_INVARIANTS: bool = cfg!(feature = "check_invariants");

/// The expected maximum number of distinct characters/rebuses/whatever appearing in a grid.
pub const MAX_GLYPH_COUNT: usize = 256;

/// The expected maximum number of slots appearing in a grid.
pub const MAX_SLOT_COUNT: usize = 256;

/// The expected maximum length for a single slot.
pub const MAX_SLOT_LENGTH: usize = 21;
