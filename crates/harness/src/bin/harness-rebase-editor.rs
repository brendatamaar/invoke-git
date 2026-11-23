//! The test binary can replace the todo for the plain-Git oracle without
//! using the app's plan serializer. Its other mode is the real app helper.

fn main() -> std::process::ExitCode {
    if let (Some(todo), Some(path)) = (
        std::env::var_os("INVOKE_TWIN_TODO"),
        std::env::args_os().nth(1),
    ) {
        match std::fs::write(path, todo.as_encoded_bytes()) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("write twin todo: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    } else {
        shell::rebase::editor_main()
    }
}
