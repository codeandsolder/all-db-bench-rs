#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    rust_db_realistic_bench::kv::concurrency::run().await
}
