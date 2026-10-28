#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> anyhow::Result<()> {
    rust_db_realistic_bench::record::sustained::run().await
}
