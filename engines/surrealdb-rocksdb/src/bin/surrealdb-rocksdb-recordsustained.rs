#![allow(clippy::too_many_lines)]

#[allow(dead_code)]
#[path = "../main.rs"]
mod base;
#[path = "../../../../src/record_sustained_driver.rs"]
mod driver;

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> anyhow::Result<()> {
    driver::run().await
}
