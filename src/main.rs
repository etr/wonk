use std::process;

fn main() {
    // Parse CLI first so we know the output format.
    // clap handles its own usage errors (exit code 2) before we get here.
    let cli = wonk::cli::parse();
    let suppress = cli.format.is_some_and(|f| f.is_structured());

    match wonk::router::dispatch(cli) {
        Ok(()) => process::exit(wonk::errors::EXIT_SUCCESS),
        Err(err) => {
            // A WonkError propagated through anyhow (e.g. a Usage error from
            // a filter parse) must keep its specific exit code; the generic
            // anyhow -> WonkError conversion would flatten it to Other (1).
            let wonk_err = match err.downcast::<wonk::errors::WonkError>() {
                Ok(specific) => specific,
                Err(err) => err.into(),
            };
            let code = wonk::output::format_error(&wonk_err, suppress);
            process::exit(code);
        }
    }
}
