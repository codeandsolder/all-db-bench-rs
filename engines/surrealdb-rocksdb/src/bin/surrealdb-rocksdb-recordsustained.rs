use all_db_bench_record_harness::sustained::{SustainedArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;
use surrealdb_rocksdb_recordbench::{Engine, EngineKind};

#[derive(Debug, Parser)]
#[command(about = "Windowed sustained-write/compaction SurrealDB/RocksDB benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: SustainedArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
