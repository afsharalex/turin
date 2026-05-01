use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::die;

/// Top-level shape of `.turin/tour.json`.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Tour {
    pub tour: TourMeta,
    pub stops: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct TourMeta {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
}

/// Read `.turin/tour.json`, exiting nonzero on any failure.
pub fn read(turin_dir: &Path) -> Tour {
    let path = turin_dir.join("tour.json");
    if !path.exists() {
        die(format!(
            "no tour at {} — run `turin new` first",
            path.display()
        ));
    }
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| die(format!("reading {}: {}", path.display(), e)));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| die(format!("parsing {}: {}", path.display(), e)))
}

/// Write `.turin/tour.json` with pretty-printed JSON.
pub fn write(turin_dir: &Path, tour: &Tour) {
    let path = turin_dir.join("tour.json");
    let text = serde_json::to_string_pretty(tour)
        .unwrap_or_else(|e| die(format!("serializing tour: {}", e)));
    fs::write(&path, format!("{}\n", text))
        .unwrap_or_else(|e| die(format!("writing {}: {}", path.display(), e)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tour(title: &str, stops: &[&str]) -> Tour {
        Tour {
            tour: TourMeta {
                title: title.into(),
                ..Default::default()
            },
            stops: stops.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn optional_metadata_omitted_when_unset() {
        let json = serde_json::to_string(&make_tour("T", &[])).unwrap();
        assert!(!json.contains("description"));
        assert!(!json.contains("author"));
        assert!(!json.contains("created"));
    }

    #[test]
    fn optional_metadata_present_when_set() {
        let t = Tour {
            tour: TourMeta {
                title: "T".into(),
                description: Some("desc".into()),
                ..Default::default()
            },
            stops: vec![],
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"description\":\"desc\""));
    }

    #[test]
    fn deserialize_tolerates_missing_optional_fields() {
        let minimal = r#"{"tour":{"title":"X"},"stops":[]}"#;
        let t: Tour = serde_json::from_str(minimal).unwrap();
        assert_eq!(t.tour.title, "X");
        assert!(t.tour.description.is_none());
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let original = make_tour("Round trip", &["a.md", "b.md"]);
        write(dir.path(), &original);
        let back = read(dir.path());
        assert_eq!(back.tour.title, "Round trip");
        assert_eq!(back.stops, vec!["a.md".to_string(), "b.md".to_string()]);
    }

    #[test]
    fn write_appends_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &make_tour("T", &[]));
        let text = fs::read_to_string(dir.path().join("tour.json")).unwrap();
        assert!(text.ends_with('\n'));
    }
}
