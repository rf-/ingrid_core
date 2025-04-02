use ingrid_core::backtracking_search::find_fill;
use ingrid_core::grid_config::{generate_grid_config_from_template_string, render_grid};
use ingrid_core::word_list::{WordList, WordListSourceConfig};
use std::collections::HashSet;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;
use wasm_bindgen::prelude::*;

const STWL_RAW: &str = include_str!("../resources/spreadthewordlist.dict");

/// WASM-compatible function to fill a crossword grid
#[wasm_bindgen]
pub fn fill_grid(
    grid_content: &str,
    min_score: Option<u16>,
    max_shared_substring: Option<usize>,
) -> Result<String, String> {
    // Normalize grid content
    let raw_grid_content = grid_content
        .trim()
        .lines()
        .map(|line| line.trim().to_lowercase().nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let height = raw_grid_content.lines().count();

    if height == 0 {
        return Err("Grid must have at least one row".into());
    }

    if raw_grid_content
        .lines()
        .map(|line| line.chars().count())
        .collect::<HashSet<_>>()
        .len()
        != 1
    {
        return Err("Rows in grid must all be the same length".into());
    }

    let width = raw_grid_content.lines().next().unwrap().chars().count() - 1;
    let max_side = width.max(height);

    if !max_shared_substring
        .map_or(true, |mss| (3..=10).contains(&mss))
    {
        return Err("If given, max shared substring must be between 3 and 10".into());
    }

    let min_score = min_score.unwrap_or(50);
    let start = Instant::now();

    // Use the embedded word list
    let word_list = WordList::new(
        vec![WordListSourceConfig::FileContents {
            id: "0".into(),
            enabled: true,
            contents: STWL_RAW,
        }],
        None,
        Some(max_side),
        max_shared_substring,
    );

    #[allow(clippy::comparison_chain)]
    if let Some(errors) = word_list.get_source_errors().get("0") {
        if errors.len() == 1 {
            return Err(format!("{}", errors[0]));
        } else if errors.len() > 1 {
            let mut full_error: String = "".into();
            for error in errors {
                full_error.push_str(&format!("\n- {error}"));
            }
            return Err(full_error);
        }
    }

    if word_list.word_id_by_string.is_empty() {
        return Err("Word list is empty".into());
    }

    let grid_config =
        generate_grid_config_from_template_string(word_list, &raw_grid_content, min_score);

    let result = find_fill(&grid_config.to_config_ref(), None, None)
        .map_err(|_| "Unfillable grid".to_string())?;

    // Return the filled grid as a string
    Ok(render_grid(&grid_config.to_config_ref(), &result.choices).replace('.', "#"))
}

