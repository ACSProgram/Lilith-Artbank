#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args_os().any(|arg| arg.to_string_lossy() == "--mcp-stdio") {
        std::process::exit(lilith_artbank_lib::run_stdio_bridge());
    }
    lilith_artbank_lib::run()
}
