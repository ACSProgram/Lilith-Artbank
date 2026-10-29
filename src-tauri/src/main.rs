#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(feature = "headless")]
    {
        let arguments = std::env::args().skip(1).collect::<Vec<_>>();
        if arguments.first().map(String::as_str) == Some("--headless") {
            std::process::exit(lilith_artbank_lib::run_headless(&arguments[1..]));
        }
    }
    if std::env::args_os().any(|arg| arg.to_string_lossy() == "--mcp-stdio") {
        std::process::exit(lilith_artbank_lib::run_stdio_bridge());
    }
    lilith_artbank_lib::run()
}
