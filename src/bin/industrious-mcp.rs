#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    industrious::process::private_std_handles();
    if let Err(error) = industrious::mcp::serve_stdio().await {
        eprintln!("oracle-mcp exited with an error: {error}");
        std::process::exit(1);
    }
}
