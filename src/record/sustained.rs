use super::engine::{Engine, EngineKind};
use all_db_bench_record_harness::sustained::{SustainedArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Windowed sustained-write/compaction benchmark for record databases")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: SustainedArgs,
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
