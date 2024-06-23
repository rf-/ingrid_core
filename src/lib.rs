#![warn(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::comparison_chain)]
#![allow(clippy::implicit_hasher)]
#![allow(clippy::match_on_vec_items)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]

pub mod arc_consistency;
pub mod backtracking_search;
pub mod dupe_index;
pub mod grid_config;
pub mod types;
pub mod util;
pub mod word_list;

/// The expected maximum number of distinct characters/rebuses/whatever appearing in a grid.
pub const MAX_GLYPH_COUNT: usize = 256;

/// The expected maximum number of slots appearing in a grid.
pub const MAX_SLOT_COUNT: usize = 256;

/// The expected maximum length for a single slot.
pub const MAX_SLOT_LENGTH: usize = 21;
