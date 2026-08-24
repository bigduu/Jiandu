//! Versioned deterministic Unicode and CJK tokenization.

use unicode_normalization::UnicodeNormalization;

/// Normalize with Unicode NFKC plus Unicode lowercase, split punctuation,
/// retain non-CJK alphanumeric words, and emit CJK unigrams plus adjacent
/// bigrams. Output order follows normalized source order exactly.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    let normalized = text.nfkc().flat_map(char::to_lowercase).collect::<String>();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut previous_cjk = None;

    for character in normalized.chars() {
        if is_cjk(character) {
            flush_word(&mut word, &mut tokens);
            if let Some(previous) = previous_cjk {
                tokens.push(format!("{previous}{character}"));
            }
            tokens.push(character.to_string());
            previous_cjk = Some(character);
        } else if character.is_alphanumeric() {
            previous_cjk = None;
            word.push(character);
        } else {
            previous_cjk = None;
            flush_word(&mut word, &mut tokens);
        }
    }
    flush_word(&mut word, &mut tokens);
    tokens
}

fn flush_word(word: &mut String, tokens: &mut Vec<String>) {
    if !word.is_empty() {
        tokens.push(std::mem::take(word));
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
            | 0x3040..=0x30FF
            | 0x31F0..=0x31FF
            | 0x1100..=0x11FF
            | 0x3130..=0x318F
            | 0xAC00..=0xD7AF
    )
}

#[cfg(test)]
mod tests {
    use super::tokenize;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TokenizerFixture {
        format_version: String,
        cases: Vec<TokenizerCase>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TokenizerCase {
        name: String,
        input: String,
        tokens: Vec<String>,
    }

    #[test]
    fn tokenization_is_nfkc_lowercased_cjk_aware_and_punctuation_explicit() {
        let bytes = include_bytes!("../fixtures/v1alpha1/tokenization.json");
        let fixture: TokenizerFixture = serde_json::from_slice(bytes).expect("strict fixture");
        assert_eq!(
            fixture.format_version,
            "jiandu.index.tokenizer-fixture/v1alpha1"
        );
        assert!(fixture.cases.len() >= 5);
        for case in fixture.cases {
            assert_eq!(tokenize(&case.input), case.tokens, "case {}", case.name);
        }
    }
}
