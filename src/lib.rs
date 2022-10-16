mod word_list;

pub const LOG_FILL_PROCESS: bool = cfg!(feature = "log_fill_process");
pub const CHECK_INVARIANTS: bool = cfg!(feature = "check_invariants");

/// The expected maximum number of distinct characters/rebuses/whatever appearing in a grid.
pub const MAX_GLYPH_COUNT: usize = 256;

/// The expected maximum number of slots appearing in a grid.
pub const MAX_SLOT_COUNT: usize = 256;

/// The expected maximum length for a single slot.
pub const MAX_SLOT_LENGTH: usize = 21;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
