use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::util::{die, slugify};

const VERSION: &str = "0.0.1";

#[derive(Parser, Debug)]
#[command(name = "turin", version = VERSION, about = "Author and play guided codebase tours")]
pub struct Cli {
    #[arg(long, global = true)]
    pub project_root: Option<PathBuf>,

    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Tour-level metadata. Used only by `new`.
#[derive(Args, Debug)]
pub struct TourArgs {
    /// Tour title.
    #[arg(long = "tour-title")]
    pub tour_title: String,

    /// Optional tour description.
    #[arg(long = "tour-description")]
    pub tour_description: Option<String>,

    /// Optional tour author.
    #[arg(long = "tour-author")]
    pub tour_author: Option<String>,

    /// Optional ISO-8601 creation date (e.g. "2026-04-29").
    #[arg(long = "tour-created")]
    pub tour_created: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum AnchorKind {
    Line,
    Pattern,
    Treesitter,
}

/// Flags describing a single stop. Shared by `new` and `add`.
#[derive(Args, Debug, Default)]
pub struct StopArgs {
    /// Path to the file the stop points at.
    #[arg(long)]
    pub file: Option<PathBuf>,

    /// Anchor kind: line, pattern, or treesitter.
    #[arg(long, value_enum)]
    pub anchor_kind: Option<AnchorKind>,

    /// Anchor value (line number, regex pattern, or tree-sitter query).
    #[arg(long)]
    pub anchor: Option<String>,

    /// Short title for the stop.
    #[arg(long)]
    pub title: Option<String>,

    /// Commentary body (inline).
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,

    /// Read commentary body from a file.
    #[arg(long, conflicts_with = "body")]
    pub body_file: Option<PathBuf>,

    /// Stable stop id, used by branches and prerequisites.
    #[arg(long)]
    pub id: Option<String>,

    /// Highlight N lines starting at the anchor.
    #[arg(long)]
    pub highlight_lines: Option<usize>,

    /// Filename slug for the stop file (e.g. "entry" -> "entry.md").
    /// Defaults from --title.
    #[arg(long)]
    pub slug: Option<String>,
}

impl StopArgs {
    /// True if any stop field was supplied.
    pub fn any_set(&self) -> bool {
        self.file.is_some()
            || self.anchor_kind.is_some()
            || self.anchor.is_some()
            || self.title.is_some()
            || self.body.is_some()
            || self.body_file.is_some()
            || self.id.is_some()
            || self.highlight_lines.is_some()
            || self.slug.is_some()
    }

    /// Names of any required fields that are not set. Empty if complete.
    pub fn missing_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.file.is_none() {
            missing.push("--file");
        }
        if self.anchor_kind.is_none() {
            missing.push("--anchor-kind");
        }
        if self.anchor.is_none() {
            missing.push("--anchor");
        }
        missing
    }

    /// Exit nonzero if any required field is missing.
    pub fn require_complete(&self) {
        let missing = self.missing_fields();
        if !missing.is_empty() {
            die(format!(
                "stop is missing required field(s): {}",
                missing.join(", ")
            ));
        }
    }

    /// Determine the slug for the stop file: --slug, else --title, else --id, else "stop".
    pub fn slug(&self) -> String {
        for source in [&self.slug, &self.title, &self.id]
            .iter()
            .filter_map(|o| o.as_ref())
        {
            let s = slugify(source);
            if !s.is_empty() {
                return s;
            }
        }
        "stop".to_string()
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scaffold a `.turin/` directory at the project root. With stop flags,
    /// seed the first stop file and add it to the index.
    New {
        #[command(flatten)]
        tour: TourArgs,

        #[command(flatten)]
        stop: StopArgs,
    },

    /// Create a new stop file and insert it into `tour.json`'s `stops` array.
    /// Without `--position`, appends to the end. Prompts for any missing fields.
    Add {
        /// 1-based position to insert the new stop at. Default: append.
        /// Valid range: 1..=len+1 (where len is the current stop count).
        #[arg(long)]
        position: Option<usize>,

        #[command(flatten)]
        stop: StopArgs,
    },

    /// Print stops as a table (index, title, file, anchor).
    List,

    /// Open the interactive TUI playback.
    Play,

    /// Print detailed usage and the on-disk format reference.
    /// Intended for humans onboarding to Turin and for LLMs that need to
    /// author stops without prior context.
    Quickstart,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> StopArgs {
        StopArgs::default()
    }

    #[test]
    fn any_set_false_by_default() {
        assert!(!empty().any_set());
    }

    #[test]
    fn any_set_true_when_any_field_set() {
        let mut s = empty();
        s.title = Some("hi".into());
        assert!(s.any_set());
    }

    #[test]
    fn missing_fields_lists_all_required() {
        let m = empty().missing_fields();
        assert_eq!(m, vec!["--file", "--anchor-kind", "--anchor"]);
    }

    #[test]
    fn missing_fields_empty_when_complete() {
        let s = StopArgs {
            file: Some(PathBuf::from("x")),
            anchor_kind: Some(AnchorKind::Pattern),
            anchor: Some("y".into()),
            ..Default::default()
        };
        assert!(s.missing_fields().is_empty());
    }

    #[test]
    fn slug_falls_back_to_title() {
        let s = StopArgs {
            title: Some("Entry Point".into()),
            ..Default::default()
        };
        assert_eq!(s.slug(), "entry-point");
    }

    #[test]
    fn slug_prefers_explicit_slug_over_title() {
        let s = StopArgs {
            slug: Some("explicit".into()),
            title: Some("Different".into()),
            ..Default::default()
        };
        assert_eq!(s.slug(), "explicit");
    }

    #[test]
    fn slug_falls_back_to_id_when_no_slug_or_title() {
        let s = StopArgs {
            id: Some("entry".into()),
            ..Default::default()
        };
        assert_eq!(s.slug(), "entry");
    }

    #[test]
    fn slug_default_is_stop() {
        assert_eq!(empty().slug(), "stop");
    }

    #[test]
    fn slug_skips_source_that_slugifies_to_empty() {
        // "!!" slugifies to "" — should fall through to next source.
        let s = StopArgs {
            slug: Some("!!".into()),
            title: Some("real title".into()),
            ..Default::default()
        };
        assert_eq!(s.slug(), "real-title");
    }
}
