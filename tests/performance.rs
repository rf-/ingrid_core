use ingrid_core::backtracking_search::find_fill;
use ingrid_core::grid_config::{generate_grid_config_from_template_string, render_grid};
use ingrid_core::word_list::{WordList, WordListSourceConfig, WordListSourceConfigProvider};
use std::time::Instant;

const STWL_RAW: &str = include_str!("../resources/spreadthewordlist.dict");

#[test]
fn test_6x6_fill_performance() {
    let start = Instant::now();

    let word_list = WordList::new(
        vec![WordListSourceConfig {
            id: "0".into(),
            enabled: true,
            provider: WordListSourceConfigProvider::FileContents { contents: STWL_RAW },
            normalization: None,
        }],
        None,
        Some(6),
        None,
    );

    let word_list_time = start.elapsed();
    println!("Word list loaded in {:?}", word_list_time);

    let template = "......\n......\n......\n......\n......\n......";
    let grid_config = generate_grid_config_from_template_string(word_list, template, 50);

    let fill_start = Instant::now();
    let result = find_fill(&grid_config.to_config_ref(), None, None).expect("Should find a fill");
    let fill_time = fill_start.elapsed();

    println!("6x6 fill found in {:?}", fill_time);
    println!("Statistics: {:?}", result.statistics);
    println!("Total time: {:?}", start.elapsed());
    println!("Result:\n{}", render_grid(&grid_config.to_config_ref(), &result.choices));
}
