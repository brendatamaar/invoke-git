//! `GIT_ASKPASS` / `SSH_ASKPASS` helper. git and ssh run it with the prompt
//! as the only argument and read the answer from stdout. It asks the app over
//! the local pipe named in the environment; a cancelled or failed request
//! exits non-zero with nothing on stdout. It never writes the answer anywhere
//! but stdout.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let address = std::env::var(askpass::ENV_ADDRESS).unwrap_or_default();
    let token = std::env::var(askpass::ENV_TOKEN).unwrap_or_default();
    match askpass::ask(&address, &token, &prompt) {
        Ok(Some(answer)) => {
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(answer.as_bytes()).is_ok()
                && stdout.write_all(b"\n").is_ok()
                && stdout.flush().is_ok()
            {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        // Cancelled by the user, or the app is gone: no secret, no output.
        Ok(None) | Err(_) => ExitCode::FAILURE,
    }
}
