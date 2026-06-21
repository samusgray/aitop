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

    /// Use simulated live agent activity instead of native local sources.
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.once {
        let snapshot = if cli.demo {
            aitop::app::demo_snapshot(0)
        } else {
            aitop::app::snapshot()?
        };
        println!("{}", snapshot.text_summary());
        return Ok(());
    }

    let source = if cli.demo {
        aitop::dashboard::DashboardSource::Demo
    } else {
        aitop::dashboard::DashboardSource::Native
    };
    aitop::dashboard::run(source)
}
