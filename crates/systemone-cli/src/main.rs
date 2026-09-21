use std::{io::IsTerminal as _, process::ExitCode};

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    // Resolve config once so output.quiet also controls native logging.
    let parsed = systemone_cli::parse(arguments);
    let quiet = match &parsed {
        systemone_cli::ParseOutcome::Ready(invocation) => invocation.resolved.config.output.quiet,
        _ => false,
    };
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_max_level(if quiet {
            tracing::Level::WARN
        } else {
            tracing::Level::INFO
        });
    let _ = subscriber.try_init();
    let stdin_handle = std::io::stdin();
    let stdin_is_terminal = stdin_handle.is_terminal();
    let mut stdin = stdin_handle.lock();
    // Do not hold a StderrLock across native inference: llama.cpp's tracing
    // callback writes from the owner thread and would deadlock against it.
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = systemone_cli::run_parsed_with_io(
        parsed,
        &mut stdin,
        stdin_is_terminal,
        &mut stdout,
        &mut stderr,
    );
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
