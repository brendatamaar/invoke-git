use invoke_core::search::{QueryError, SearchField, SearchQuery};
use proptest::prelude::*;

proptest! {
    #[test]
    fn hashes_match_only_at_the_start(
        tail in "[0-9a-f]{39}|[0-9a-f]{63}",
        width in 1..=64_usize,
    ) {
        let hash = format!("a{tail}");
        let prefix = &hash[..width.min(hash.len())];
        let query = SearchQuery::parse(SearchField::Hash, &prefix.to_uppercase()).unwrap();
        prop_assert!(query.matches_hash(&hash));
        let different = format!("b{hash}");
        prop_assert!(!query.matches_hash(&different));
    }

    #[test]
    fn literal_text_matches_in_the_selected_field(term in "[a-z]{1,20}") {
        let mixed_case = term.to_uppercase();
        let value = format!("!{mixed_case}!");
        let message = SearchQuery::parse(SearchField::Message, &format!("  {term} ")).unwrap();
        let author = SearchQuery::parse(SearchField::Author, &term).unwrap();
        prop_assert!(message.matches_text(&value, "", ""));
        prop_assert!(!message.matches_text("", &value, &value));
        prop_assert!(author.matches_text("", &value, ""));
        prop_assert!(author.matches_text("", "", &value));
        prop_assert!(!author.matches_text(&value, "", ""));
    }

    #[test]
    fn path_matches_exactly_when_an_entry_changes(
        path in "[a-z]{1,8}/[A-Z]{1,8} [a-z]{1,8}",
        entry in prop::option::of(any::<u8>()),
        new_entry in prop::option::of(any::<u8>()),
    ) {
        let query = SearchQuery::parse(SearchField::Path, &path).unwrap();
        prop_assert_eq!(query.path(), Some(path.as_str()));
        prop_assert_eq!(query.matches_path_change(entry, new_entry), entry != new_entry);
    }
}

#[test]
fn invalid_filters_report_the_problem() {
    for (field, input, error) in [
        (SearchField::Message, " \n", QueryError::Empty),
        (SearchField::Author, "", QueryError::Empty),
        (SearchField::Hash, "123g", QueryError::InvalidHash),
        (SearchField::Path, "../file", QueryError::InvalidPath),
        (SearchField::Path, "/file", QueryError::InvalidPath),
        (SearchField::Path, "a//b", QueryError::InvalidPath),
        (SearchField::Path, "a\0b", QueryError::InvalidPath),
    ] {
        assert_eq!(SearchQuery::parse(field, input), Err(error));
    }
    assert_eq!(
        SearchQuery::parse(SearchField::Hash, &"a".repeat(65)),
        Err(QueryError::InvalidHash)
    );
}
