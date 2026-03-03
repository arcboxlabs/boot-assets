mod cmd;
mod release;
mod rootfs;

use clap::Parser;
use cmd::Cli;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli.run()
}
