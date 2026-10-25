mod app;
mod data;
mod git;
mod ui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::run().await
}
