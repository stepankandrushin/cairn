fn main() -> std::process::ExitCode {
    // The harness's own list of roots, and on macOS and Windows its daemons' locks and sockets, never
    // the user's. A test that sets its own gives it to every process it starts.
    if std::env::var_os("CAIRN_USER_DIR").is_none() {
        std::env::set_var("CAIRN_USER_DIR", std::env::temp_dir().join("cairn-h"));
    }
    cairn_cli::run()
}
