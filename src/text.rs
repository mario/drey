//! Document text with LSP position math.
//!
//! LSP positions are (line, character) where `character` counts UTF-16 code
//! units, not bytes and not chars. Getting this wrong silently desynchronises
//! a shadow from the server, so it is worth the explicit conversion.

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
    pub content: String,
}

impl Text {
    pub fn new(content: String) -> Self {
        Self { content }
    }

    /// Byte offset of a UTF-16 LSP position, clamped to the document.
    fn offset_of(&self, line: u32, character: u32) -> usize {
        let mut idx = 0usize;

        if line > 0 {
            let mut seen = 0u32;
            let mut found = false;
            for (i, b) in self.content.bytes().enumerate() {
                if b == b'\n' {
                    seen += 1;
                    if seen == line {
                        idx = i + 1;
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                return self.content.len();
            }
        }

        let mut u16_seen = 0u32;
        for (rel, ch) in self.content[idx..].char_indices() {
            if u16_seen >= character || ch == '\n' {
                return idx + rel;
            }
            u16_seen += ch.len_utf16() as u32;
        }
        self.content.len()
    }

    /// Applies one entry of `contentChanges`. A change with no `range` is a
    /// full-document replacement.
    pub fn apply(&mut self, change: &Value) {
        let new_text = change
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let Some(range) = change.get("range") else {
            self.content = new_text.to_string();
            return;
        };

        let pos = |k: &str| -> (u32, u32) {
            let p = range.get(k);
            let n = |f: &str| {
                p.and_then(|p| p.get(f))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32
            };
            (n("line"), n("character"))
        };
        let (sl, sc) = pos("start");
        let (el, ec) = pos("end");

        let start = self.offset_of(sl, sc);
        let end = self.offset_of(el, ec).max(start);
        self.content.replace_range(start..end, new_text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn change(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> Value {
        json!({
            "range": { "start": {"line": sl, "character": sc},
                       "end":   {"line": el, "character": ec} },
            "text": text
        })
    }

    #[test]
    fn full_replacement_when_range_absent() {
        let mut t = Text::new("old".into());
        t.apply(&json!({ "text": "new" }));
        assert_eq!(t.content, "new");
    }

    #[test]
    fn single_line_edit() {
        let mut t = Text::new("let x = 1;".into());
        t.apply(&change(0, 8, 0, 9, "42"));
        assert_eq!(t.content, "let x = 42;");
    }

    #[test]
    fn multi_line_edit() {
        let mut t = Text::new("a\nb\nc\n".into());
        t.apply(&change(0, 1, 2, 0, "X"));
        assert_eq!(t.content, "aXc\n");
    }

    #[test]
    fn insertion_at_empty_range() {
        let mut t = Text::new("ac".into());
        t.apply(&change(0, 1, 0, 1, "b"));
        assert_eq!(t.content, "abc");
    }

    #[test]
    fn positions_count_utf16_units_not_chars() {
        // A crab is one char but two UTF-16 units.
        let mut t = Text::new("x🦀y".into());
        t.apply(&change(0, 3, 0, 4, "Z"));
        assert_eq!(t.content, "x🦀Z");
    }

    #[test]
    fn character_past_end_of_line_clamps_to_line_end() {
        let mut t = Text::new("ab\ncd".into());
        t.apply(&change(0, 99, 0, 99, "!"));
        assert_eq!(t.content, "ab!\ncd");
    }

    #[test]
    fn line_past_end_clamps_to_document_end() {
        let mut t = Text::new("ab".into());
        t.apply(&change(9, 9, 9, 9, "!"));
        assert_eq!(t.content, "ab!");
    }

    #[test]
    fn inverted_range_does_not_panic() {
        let mut t = Text::new("abcdef".into());
        t.apply(&change(0, 4, 0, 1, "Z"));
        assert_eq!(t.content, "abcdZef");
    }

    #[test]
    fn sequential_changes_compose() {
        let mut t = Text::new("fn main() {}".into());
        t.apply(&change(0, 11, 0, 11, "\n    todo!();\n"));
        t.apply(&change(1, 4, 1, 8, "done"));
        assert_eq!(t.content, "fn main() {\n    done!();\n}");
    }

    #[test]
    fn empty_document_accepts_an_insertion_at_the_origin() {
        let mut t = Text::default();
        t.apply(&change(0, 0, 0, 0, "hello"));
        assert_eq!(t.content, "hello");
    }

    #[test]
    fn empty_document_clamps_any_position() {
        let mut t = Text::new(String::new());
        t.apply(&change(4, 7, 9, 2, "x"));
        assert_eq!(t.content, "x");
    }

    #[test]
    fn deleting_a_whole_document_leaves_it_empty() {
        let mut t = Text::new("a\nb\nc".into());
        t.apply(&change(0, 0, 2, 1, ""));
        assert_eq!(t.content, "");
    }

    #[test]
    fn crlf_carriage_returns_count_as_characters() {
        // LSP lines are delimited by \n; the \r belongs to the preceding line
        // and occupies a UTF-16 unit like anything else.
        let mut t = Text::new("ab\r\ncd".into());
        t.apply(&change(1, 0, 1, 1, "X"));
        assert_eq!(t.content, "ab\r\nXd");
    }

    #[test]
    fn a_position_at_the_crlf_line_end_does_not_eat_the_carriage_return() {
        let mut t = Text::new("ab\r\ncd".into());
        t.apply(&change(0, 2, 0, 2, "!"));
        assert_eq!(t.content, "ab!\r\ncd");
    }

    #[test]
    fn astral_characters_occupy_two_utf16_units() {
        let mut t = Text::new("🦀🦀".into());
        // Character 2 is the boundary between the two crabs.
        t.apply(&change(0, 2, 0, 2, "|"));
        assert_eq!(t.content, "🦀|🦀");
    }

    #[test]
    fn a_position_inside_a_surrogate_pair_snaps_to_the_char_boundary() {
        // Character 1 is halfway through a crab. Splitting the string there
        // would panic, so it must round to a boundary instead.
        let mut t = Text::new("🦀x".into());
        t.apply(&change(0, 1, 0, 1, "!"));
        assert!(t.content == "!🦀x" || t.content == "🦀!x", "{}", t.content);
    }

    #[test]
    fn multi_byte_bmp_characters_are_one_unit_each() {
        // é is two bytes but one UTF-16 unit.
        let mut t = Text::new("éé".into());
        t.apply(&change(0, 1, 0, 2, "Z"));
        assert_eq!(t.content, "éZ");
    }

    #[test]
    fn a_line_ending_edit_can_join_two_lines() {
        let mut t = Text::new("a\nb".into());
        t.apply(&change(0, 1, 1, 0, ""));
        assert_eq!(t.content, "ab");
    }

    #[test]
    fn a_trailing_newline_makes_one_more_empty_line() {
        let mut t = Text::new("a\n".into());
        t.apply(&change(1, 0, 1, 0, "b"));
        assert_eq!(t.content, "a\nb");
    }

    #[test]
    fn a_change_without_text_deletes_the_range() {
        let mut t = Text::new("abc".into());
        t.apply(&json!({
            "range": { "start": {"line": 0, "character": 0},
                       "end":   {"line": 0, "character": 2} }
        }));
        assert_eq!(t.content, "c");
    }

    #[test]
    fn a_malformed_range_is_treated_as_the_origin() {
        let mut t = Text::new("abc".into());
        t.apply(&json!({ "range": { "start": {}, "end": {} }, "text": "Z" }));
        assert_eq!(t.content, "Zabc");
    }

    #[test]
    fn an_explicit_null_range_is_not_a_full_replacement() {
        // A present-but-null `range` still takes the ranged branch, where the
        // missing line/character read as zero. Pinned because it is the one
        // shape where "no range" and "range: null" part ways.
        let mut t = Text::new("abc".into());
        t.apply(&json!({ "range": null, "text": "Z" }));
        assert_eq!(t.content, "Zabc");
    }

    #[test]
    fn documents_compare_by_content() {
        assert_eq!(Text::new("a".into()), Text::new("a".into()));
        assert_ne!(Text::new("a".into()), Text::new("b".into()));
    }

    /// Independent implementation of LSP position semantics, deliberately
    /// written with chars rather than byte scanning so it cannot share a bug
    /// with the code under test.
    fn reference_apply(text: &str, sl: u32, sc: u32, el: u32, ec: u32, new_text: &str) -> String {
        let offset = |line: u32, character: u32| -> usize {
            let mut idx = 0usize;
            for _ in 0..line {
                match text[idx..].find('\n') {
                    Some(i) => idx += i + 1,
                    None => return text.len(),
                }
            }
            let mut units = 0u32;
            for (rel, ch) in text[idx..].char_indices() {
                if units >= character || ch == '\n' {
                    return idx + rel;
                }
                units += ch.len_utf16() as u32;
            }
            text.len()
        };
        let start = offset(sl, sc);
        let end = offset(el, ec).max(start);
        format!("{}{}{}", &text[..start], new_text, &text[end..])
    }

    proptest::proptest! {
        #[test]
        fn a_full_replacement_always_wins(before: String, after: String) {
            let mut t = Text::new(before);
            t.apply(&json!({ "text": after.clone() }));
            proptest::prop_assert_eq!(t.content, after);
        }

        #[test]
        fn any_single_edit_matches_the_reference_implementation(
            content in "\\PC{0,60}",
            sl in 0u32..4, sc in 0u32..8, el in 0u32..4, ec in 0u32..8,
            new_text in "\\PC{0,10}",
        ) {
            let expected = reference_apply(&content, sl, sc, el, ec, &new_text);
            let mut t = Text::new(content);
            t.apply(&change(sl, sc, el, ec, &new_text));
            proptest::prop_assert_eq!(t.content, expected);
        }

        #[test]
        fn a_sequence_of_edits_matches_the_reference_implementation(
            content in "[a\nb\u{e9}\u{1f980}]{0,40}",
            edits in proptest::collection::vec(
                (0u32..4, 0u32..6, 0u32..4, 0u32..6, "[xy\n]{0,4}"), 1..8),
        ) {
            let mut expected = content.clone();
            for (sl, sc, el, ec, new_text) in &edits {
                expected = reference_apply(&expected, *sl, *sc, *el, *ec, new_text);
            }
            let mut t = Text::new(content);
            for (sl, sc, el, ec, new_text) in &edits {
                t.apply(&change(*sl, *sc, *el, *ec, new_text));
            }
            proptest::prop_assert_eq!(t.content, expected);
        }

        #[test]
        fn an_empty_range_edit_only_inserts(
            content in "[ab\n\u{1f980}]{0,30}", line in 0u32..3, ch in 0u32..6,
        ) {
            let mut t = Text::new(content.clone());
            t.apply(&change(line, ch, line, ch, "MARK"));
            proptest::prop_assert_eq!(t.content.len(), content.len() + 4);
            proptest::prop_assert!(t.content.contains("MARK"));
        }
    }
}
