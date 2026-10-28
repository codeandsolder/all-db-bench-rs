use super::engine::{Engine, EngineKind};
use all_db_bench_record_harness::concurrency::{ConcurrencyArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Shared-database multi-client record/document DB scaling benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: ConcurrencyArgs,
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
