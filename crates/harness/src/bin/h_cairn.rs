fn main() -> std::process::ExitCode {
    // The harness's own list of roots, and on macOS and Windows its daemons' locks and sockets, never
    // the user's: one for each folder of roots, a test's sandbox, as the devices of a test share it. A
    // test that sets its own gives it to every process it starts. Empty is unset, as
    // `cairn_protocol::local::user_dir` reads it, or the user's own directory would be used after all.
    if std::env::var_os("CAIRN_USER_DIR").is_none_or(|v| v.is_empty()) {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let dir = cairn_harness::root_in(&args)
            .and_then(|root| root.parent().map(cairn_harness::user_dir_of))
            .unwrap_or_else(cairn_harness::user_dir);
        std::env::set_var("CAIRN_USER_DIR", dir);
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
