use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cli::{AnchorKind, StopArgs, TourArgs};
use crate::play;
use crate::stop;
use crate::tour::{self, Tour, TourMeta};
use crate::util::{die, prompt};

const QUICKSTART: &str = include_str!("quickstart.txt");

pub fn quickstart() {
    print!("{}", QUICKSTART);
}

pub fn new(turin_dir: &Path, tour_args: TourArgs, stop: StopArgs) {
    let tour_path = turin_dir.join("tour.json");
    if tour_path.exists() {
        die(format!("{} already exists", tour_path.display()));
    }
    fs::create_dir_all(turin_dir)
        .unwrap_or_else(|e| die(format!("creating {}: {}", turin_dir.display(), e)));

    let stops = if stop.any_set() {
        stop.require_complete();
        vec![stop::write_file(turin_dir, &stop)]
    } else {
        Vec::new()
    };

    let tour = Tour {
        tour: TourMeta {
            title: tour_args.tour_title,
            description: tour_args.tour_description,
            author: tour_args.tour_author,
            created: tour_args.tour_created,
        },
        stops: stops.clone(),
    };
    tour::write(turin_dir, &tour);

    println!("created {}", tour_path.display());
    for s in &stops {
        println!("  + {}", s);
    }
}

pub fn add(turin_dir: &Path, mut stop: StopArgs, position: Option<usize>) {
    let mut tour = tour::read(turin_dir);

    interactive_complete(&mut stop);
    stop.require_complete();

    // Validate position before writing the stop file so a bad index doesn't
    // leave an orphan file on disk.
    let insert_at = match position {
        Some(p) => {
            let max = tour.stops.len() + 1;
            if !(1..=max).contains(&p) {
                die(format!(
                    "--position must be in 1..={} (current index has {} stop{})",
                    max,
                    tour.stops.len(),
                    if tour.stops.len() == 1 { "" } else { "s" }
                ));
            }
            p - 1
        }
        None => tour.stops.len(),
    };

    let filename = stop::write_file(turin_dir, &stop);
    if tour.stops.contains(&filename) {
        die(format!(
            "{} is already listed in tour.json's stops array",
            filename
        ));
    }
    tour.stops.insert(insert_at, filename.clone());
    tour::write(turin_dir, &tour);

    if position.is_some() {
        println!("inserted {} at position {}", filename, insert_at + 1);
    } else {
        println!("appended {}", filename);
    }
}

pub fn list(turin_dir: &Path) {
    let tour = tour::read(turin_dir);

    println!(
        "{} ({} stop{})",
        tour.tour.title,
        tour.stops.len(),
        if tour.stops.len() == 1 { "" } else { "s" }
    );
    if let Some(d) = &tour.tour.description {
        println!("{}", d);
    }
    if tour.stops.is_empty() {
        return;
    }

    println!();
    println!("{:<3}  {:<28}  {:<36}  anchor", "#", "title", "file");
    println!("{}", "-".repeat(90));
    for (i, filename) in tour.stops.iter().enumerate() {
        let path: PathBuf = turin_dir.join(filename);
        let fm = stop::parse_frontmatter(&path);
        println!(
            "{:<3}  {:<28}  {:<36}  {}",
            i + 1,
            truncate(fm.title.as_deref().unwrap_or(filename), 28),
            truncate(&fm.file, 36),
            fm.anchor_display(),
        );
    }
}

pub fn play(turin_dir: &Path, color: bool) {
    use crossterm::event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyEventKind,
    };
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    let mut player = play::load(turin_dir, color);

    enable_raw_mode().unwrap_or_else(|e| die(format!("enabling raw mode: {}", e)));
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        die(format!("entering alternate screen: {}", e));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            die(format!("creating terminal: {}", e));
        }
    };

    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|f| play::render(&player, f))?;
            match event::read()? {
                CtEvent::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if let Some(ev) = play::key_to_event(key)
                        && matches!(player.handle(ev), play::Outcome::Exit)
                    {
                        return Ok(());
                    }
                }
                CtEvent::Mouse(mouse) => {
                    let area = terminal.size()?.into();
                    if let Some(ev) = play::mouse_to_event(mouse, area)
                        && matches!(player.handle(ev), play::Outcome::Exit)
                    {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    })();

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();

    if let Err(e) = result {
        die(format!("play: {}", e));
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Fill in any required fields by prompting on stdin. Optional fields are
/// not prompted.
fn interactive_complete(stop: &mut StopArgs) {
    if stop.file.is_none() {
        let v = prompt("file path: ");
        if !v.is_empty() {
            stop.file = Some(PathBuf::from(v));
        }
    }
    if stop.anchor_kind.is_none() {
        loop {
            let v = prompt("anchor kind [line/pattern/treesitter] (default: pattern): ");
            let v = if v.is_empty() {
                "pattern".to_string()
            } else {
                v
            };
            match v.as_str() {
                "line" => {
                    stop.anchor_kind = Some(AnchorKind::Line);
                    break;
                }
                "pattern" => {
                    stop.anchor_kind = Some(AnchorKind::Pattern);
                    break;
                }
                "treesitter" => {
                    stop.anchor_kind = Some(AnchorKind::Treesitter);
                    break;
                }
                _ => eprintln!("  unknown kind; pick line, pattern, or treesitter"),
            }
        }
    }
    if stop.anchor.is_none() {
        let label = match stop.anchor_kind {
            Some(AnchorKind::Line) => "anchor (line number): ",
            Some(AnchorKind::Pattern) => "anchor (regex pattern): ",
            Some(AnchorKind::Treesitter) => "anchor (tree-sitter query): ",
            None => "anchor: ",
        };
        let v = prompt(label);
        if !v.is_empty() {
            stop.anchor = Some(v);
        }
    }
    if stop.title.is_none() {
        let v = prompt("title (optional): ");
        if !v.is_empty() {
            stop.title = Some(v);
        }
    }
}
