#[path = "support/bamboo_memory_corpus.rs"]
mod bamboo_memory_corpus;

use std::fs;

#[test]
fn bamboo_memory_manifest_is_byte_deterministic() {
    let generated = bamboo_memory_corpus::render_generated_manifest()
        .expect("generate Bamboo compatibility corpus manifest");
    let checked_in = fs::read_to_string(bamboo_memory_corpus::manifest_path())
        .expect("read checked-in Bamboo compatibility corpus manifest");
    assert_eq!(
        checked_in, generated,
        "run `cargo run -p jiandu-core --example generate_bamboo_memory_fixture_manifest --quiet` and replace only the checked-in Jiandu manifest"
    );
}

#[test]
fn bamboo_memory_corpus_is_complete_sanitized_and_contract_valid() {
    bamboo_memory_corpus::validate_corpus().expect("validate Bamboo compatibility corpus");
}
