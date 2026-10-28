use super::engine::{Engine, EngineKind};
use all_db_bench_record_harness::single::{SingleArgs, run as run_harness};
use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "End-to-end record/document DB benchmark; separate from raw KV results")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[command(flatten)]
    common: SingleArgs,
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    run_harness::<Engine>(args.engine, args.common).await
}
