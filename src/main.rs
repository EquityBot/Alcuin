#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    alcuin::process::private_std_handles();
    let code = match alcuin::cli::run(std::env::args_os()).await {
        Ok(code) => code,
        Err(error) => {
            if let Some(duplicate) = error.downcast_ref::<alcuin::execution::DuplicatePrompt>()
            {
                println!("{duplicate}");
            } else if !error.is::<alcuin::execution::LoggedFailure>() {
                eprintln!("✖ {error:#}");
            }
            1
        }
    };
    std::process::exit(code);
}
