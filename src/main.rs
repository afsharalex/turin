mod cli;
mod cmd;
mod play;
mod stop;
mod tour;
mod util;

use crate::cli::Command;

fn main() {
    let cli = cli::parse();
    let root = util::project_root(cli.project_root.as_deref());
    let turin_dir = root.join(".turin");

    match cli.command {
        Command::New { tour, stop } => cmd::new(&turin_dir, tour, stop),
        Command::Add { stop, position } => cmd::add(&turin_dir, stop, position),
        Command::List => cmd::list(&turin_dir),
        Command::Play => cmd::play(&turin_dir, !cli.no_color),
        Command::Quickstart => cmd::quickstart(),
    }
}
