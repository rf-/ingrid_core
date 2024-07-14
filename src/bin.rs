use clap::Parser;
use ingrid_core::backtracking_search::find_fill;
use ingrid_core::grid_config::{generate_grid_config_from_template_string, render_grid};
use ingrid_core::word_list::{WordList, WordListSourceConfig};
use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::fs;
use unicode_normalization::UnicodeNormalization;

const STWL_RAW: &str = include_str!("../resources/spreadthewordlist.dict");

/// ingrid_core: Command-line crossword generation tool
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the grid file, as ASCII with # representing blocks and . representing empty squares
    grid_path: String,

    /// Path to a scored wordlist file [default: (embedded copy of Spread the Wordlist)]
    #[arg(long)]
    wordlist: Option<String>,

    /// Minimum allowable word score
    #[arg(long, default_value_t = 50)]
    min_score: u16,

    /// Maximum shared substring length between entries [default: none]
    #[arg(long)]
    max_shared_substring: Option<usize>,
}

struct Error(String);

impl Debug for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0) // Print error unquoted
    }
}

fn main() -> Result<(), Error> {
    let args = Args::parse();

    let raw_grid_content = fs::read_to_string(&args.grid_path)
        .map_err(|_| Error(format!("Couldn't read file '{}'", args.grid_path)))?
        .trim()
        .lines()
        .map(|line| line.trim().to_lowercase().nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let height = raw_grid_content.lines().count();

    if height == 0 {
        return Err(Error("Grid must have at least one row".into()));
    }

    if raw_grid_content
        .lines()
        .map(|line| line.chars().count())
        .collect::<HashSet<_>>()
        .len()
        != 1
    {
        return Err(Error("Rows in grid must all be the same length".into()));
    }

    let width = raw_grid_content.lines().next().unwrap().chars().count() - 1;
    let max_side = width.max(height);

    if !args
        .max_shared_substring
        .map_or(true, |mss| (3..=10).contains(&mss))
    {
        return Err(Error(
            "If given, max shared substring must be between 3 and 10".into(),
        ));
    }

    let word_list = WordList::new(
        vec![match args.wordlist {
            Some(wordlist_path) => WordListSourceConfig::File {
                id: "0".into(),
                enabled: true,
                path: wordlist_path.into(),
            },
            None => WordListSourceConfig::FileContents {
                id: "0".into(),
                enabled: true,
                contents: STWL_RAW,
            },
        }],
        None,
        Some(max_side),
        args.max_shared_substring,
    );

    #[allow(clippy::comparison_chain)]
    if let Some(errors) = word_list.get_source_errors().get("0") {
        if errors.len() == 1 {
            return Err(Error(format!("{}", errors[0])));
        } else if errors.len() > 1 {
            let mut full_error: String = "".into();
            for error in errors {
                full_error.push_str(&format!("\n- {error}"));
            }
            return Err(Error(full_error));
        }
    }

    if word_list.word_id_by_string.is_empty() {
        return Err(Error("Word list is empty".into()));
    }

    let grid_config =
        generate_grid_config_from_template_string(word_list, &raw_grid_content, args.min_score);

    let result = find_fill(&grid_config.to_config_ref(), None, None)
        .map_err(|_| Error("Unfillable grid".into()))?;

    println!(
        "{}",
        render_grid(&grid_config.to_config_ref(), &result.choices).replace('.', "#")
    );

    Ok(())
}
