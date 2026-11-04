fn main() -> std::process::ExitCode {
    // The harness's own list of roots, and on macOS and Windows its daemons' locks and sockets, never
    // the user's. A test that sets its own gives it to every process it starts. Empty is unset, as
    // `cairn_protocol::local::user_dir` reads it, or the user's own directory would be used after all.
    if std::env::var_os("CAIRN_USER_DIR").is_none_or(|v| v.is_empty()) {
        std::env::set_var("CAIRN_USER_DIR", cairn_harness::user_dir());
    }
    cairn_cli::run()
}
