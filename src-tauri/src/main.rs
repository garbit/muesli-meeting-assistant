// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--import" {
        // Headless mode for integration tests and CI: process a file, print the transcript.
        std::process::exit(muesli_lib::headless::import_and_process(&args[2]));
    }
    if args.len() >= 2 && args[1] == "--doctor" {
        // Print a diagnostics report for the real library (models, meetings, jobs, log tail).
        std::process::exit(muesli_lib::diagnostics::doctor());
    }
    if args.len() >= 4 && args[1] == "--capture-test" {
        // Record N seconds of mic + system audio into a directory and write capture-test.json there.
        // Launch via `open -a Muesli.app --args --capture-test 8 /tmp/dir` so macOS attributes the
        // permission prompts to Muesli rather than the terminal.
        std::process::exit(muesli_lib::headless::capture_test(&args[2], &args[3]));
    }
    muesli_lib::run()
}
