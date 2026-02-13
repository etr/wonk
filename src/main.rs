use std::process;

fn main() {
    // Parse CLI first so we know whether --json was requested.
    // clap handles its own usage errors (exit code 2) before we get here.
    let cli = wonk::cli::parse();
    let json = cli.json;

    match wonk::router::dispatch(cli) {
        Ok(()) => process::exit(wonk::errors::EXIT_SUCCESS),
        Err(err) => {
            let wonk_err: wonk::errors::WonkError = err.into();
            let code = wonk::output::format_error(&wonk_err, json);
            process::exit(code);
        }
    }
}
