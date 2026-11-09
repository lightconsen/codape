//! Hidden helper that compiles one WASM module out-of-process.
//!
//! Usage: `codape-wasm-compile <input> <output>`
//!
//! The runtime no longer requires this binary: the Codape CLI injects its own
//! executable with the equivalent `--internal-wasm-compile` argument. This
//! helper remains for tests and for hosts that prefer a dedicated compiler
//! process.
//!
//! The output file is `wasmtime::Module::serialize` bytes; the parent
//! deserializes and AOT-caches them.

fn main() {
    if let Err(err) = codape_plugins::wasm_runtime::compile_helper_main(std::env::args_os().skip(1))
    {
        eprintln!("codape-wasm-compile: {err}");
        std::process::exit(1);
    }
}
