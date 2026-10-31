// No console window in release; `--probe` writes its log to a file instead.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Anything that looks like a diagnostic flag goes to the CLI path; only a
    // bare invocation opens the window.
    //
    // `--probe` is the default branch (see `diagnostic`), listed here only so
    // naming it explicitly does the same thing as naming nothing.
    const DIAG_FLAGS: [&str; 21] = [
        "--probe", "--cmd", "--sleep", "--rawcmd", "--conn", "--bench", "--dump", "--ble",
        "--hids", "--pairloop", "--fwbin", "--rate", "--rgb", "--advanced", "--newfeatures",
        "--verify", "--check", "--commit", "--macro", "--stage", "--persist",
    ];
    if args.iter().any(|a| DIAG_FLAGS.contains(&a.as_str())) {
        p1studio_lib::diagnostic(&args);
        return;
    }
    p1studio_lib::run()
}
