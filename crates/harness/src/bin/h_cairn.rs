fn main() -> std::process::ExitCode {
    // The harness's own list of roots, and on macOS and Windows its daemons' locks and sockets, never
    // the user's. A test that sets its own gives it to every process it starts. Empty is unset, as
    // `cairn_protocol::local::user_dir` reads it, or the user's own directory would be used after all.
    if std::env::var_os("CAIRN_USER_DIR").is_none_or(|v| v.is_empty()) {
        std::env::set_var("CAIRN_USER_DIR", cairn_harness::user_dir());
    }
    // A request the network failed is tried again after a millisecond, not half a second and up: a test
    // whose server is down would otherwise wait a minute for the sync to fail. A test that tunes this
    // itself keeps its own.
    let tune = std::env::var("CAIRN_FP_TUNE").unwrap_or_default();
    if !tune.split(',').any(|t| t.starts_with("client.retry_ms=")) {
        let sep = if tune.is_empty() { "" } else { "," };
        std::env::set_var("CAIRN_FP_TUNE", format!("{tune}{sep}client.retry_ms=1"));
    }
    cairn_cli::run()
}
