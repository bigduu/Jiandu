#[path = "../tests/support/bamboo_memory_corpus.rs"]
mod bamboo_memory_corpus;

use std::fs;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [] => {
            let manifest = bamboo_memory_corpus::render_generated_manifest()
                .expect("generate Bamboo compatibility corpus manifest");
            print!("{manifest}");
        }
        [flag] if flag == "--check" => {
            bamboo_memory_corpus::validate_corpus().expect("validate Bamboo compatibility corpus");
            let generated = bamboo_memory_corpus::render_generated_manifest()
                .expect("generate Bamboo compatibility corpus manifest");
            let checked_in = fs::read_to_string(bamboo_memory_corpus::manifest_path())
                .expect("read checked-in Bamboo compatibility corpus manifest");
            assert_eq!(checked_in, generated, "checked-in manifest is stale");
        }
        _ => {
            eprintln!("usage: generate_bamboo_memory_fixture_manifest [--check]");
            std::process::exit(2);
        }
    }
}
