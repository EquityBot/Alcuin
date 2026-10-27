#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    industrious::process::private_std_handles();
    let code = match industrious::cli::run(std::env::args_os()).await {
        Ok(code) => code,
        Err(error) => {
            if let Some(duplicate) = error.downcast_ref::<industrious::execution::DuplicatePrompt>()
            {
                println!("{duplicate}");
            } else if !error.is::<industrious::execution::LoggedFailure>() {
                eprintln!("✖ {error:#}");
            }
            1
        }
    };
    std::process::exit(code);
}
