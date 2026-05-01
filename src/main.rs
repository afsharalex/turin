mod cli;
mod cmd;
mod play;
mod stop;
mod tour;
mod util;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let root = util::project_root(cli.project_root.as_deref());
    let turin_dir = root.join(".turin");

    match cli.command {
        Command::New { tour, stop } => cmd::new(&turin_dir, tour, stop),
        Command::Add { stop, position } => cmd::add(&turin_dir, stop, position),
        Command::List => cmd::list(&turin_dir),
        Command::Play => cmd::play(&turin_dir),
        Command::Quickstart => cmd::quickstart(),
    }
}
