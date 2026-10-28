use all_db_bench_record_harness::concurrency::{ConcurrencyArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;
use surrealdb_rocksdb_recordbench::{Engine, EngineKind};

#[derive(Debug, Parser)]
#[command(about = "Shared-database multi-client SurrealDB/RocksDB record scaling benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: ConcurrencyArgs,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
