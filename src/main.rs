use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Ambient terminal monitor for local AI agent activity"
)]
struct Cli {
    /// Print one ambient snapshot instead of opening the TUI.
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.once {
        let snapshot = aitop::app::snapshot()?;
        println!("{}", snapshot.text_summary());
        return Ok(());
    }

    aitop::dashboard::run()
}
