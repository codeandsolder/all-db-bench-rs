use all_db_bench_record_harness::single::{SingleArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;
use surrealdb_rocksdb_recordbench::{Engine, EngineKind};

#[derive(Debug, Parser)]
#[command(about = "End-to-end SurrealDB/RocksDB record benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: SingleArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
