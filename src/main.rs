use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    ig_reels_scroll::app::run().await
}
