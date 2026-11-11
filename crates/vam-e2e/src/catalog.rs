//! Loading `e2e/scenarios/*.yaml` and `e2e/world/*.yaml`.

use crate::scenario::Scenario;
use crate::world::WorldPiece;
use std::fmt;
use std::path::{Path, PathBuf};

/// One problem found in the catalog: `path: rule: message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckError {
    pub path: String,
    pub rule: &'static str,
    pub message: String,
}

impl CheckError {
    pub fn new(path: impl Into<String>, rule: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            rule,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.path, self.rule, self.message)
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub scenarios: Vec<(PathBuf, Scenario)>,
    pub pieces: Vec<(PathBuf, WorldPiece)>,
    /// Parse and IO errors found while loading.
    pub errors: Vec<CheckError>,
}

fn yaml_files(dir: &Path, errors: &mut Vec<CheckError>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "yaml" || x == "yml") {
                    out.push(p);
                }
            }
        }
        Err(e) => errors.push(CheckError::new(
            dir.display().to_string(),
            "io",
            e.to_string(),
        )),
    }
    out.sort();
    out
}

fn load_one<T: serde::de::DeserializeOwned>(
    path: &Path,
    errors: &mut Vec<CheckError>,
) -> Option<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            errors.push(CheckError::new(
                path.display().to_string(),
                "io",
                e.to_string(),
            ));
            return None;
        }
    };
    match serde_yaml::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(CheckError::new(
                path.display().to_string(),
                "schema",
                e.to_string(),
            ));
            None
        }
    }
}

impl Catalog {
    /// Loads everything under `root`; problems are collected in `errors`.
    pub fn load(root: &Path) -> Catalog {
        let mut c = Catalog::default();
        for p in yaml_files(&root.join("scenarios"), &mut c.errors) {
            if let Some(s) = load_one::<Scenario>(&p, &mut c.errors) {
                c.scenarios.push((p, s));
            }
        }
        for p in yaml_files(&root.join("world"), &mut c.errors) {
            if let Some(mut w) = load_one::<WorldPiece>(&p, &mut c.errors) {
                w.fixture_dir = w
                    .fixture
                    .as_ref()
                    .map(|f| root.join("world").join("fixtures").join(f));
                if w.fixture_dir.is_none() && !w.files.is_empty() {
                    w.fixture_dir = Some(root.join("world").join("fixtures"));
                }
                c.pieces.push((p, w));
            }
        }
        c
    }

    pub fn piece(&self, name: &str) -> Option<&WorldPiece> {
        self.pieces.iter().map(|(_, w)| w).find(|w| w.piece == name)
    }

    /// Scenarios selected by ids and/or tags (all when both are empty).
    pub fn select(&self, ids: &[String], tags: &[String]) -> Vec<&Scenario> {
        self.scenarios
            .iter()
            .map(|(_, s)| s)
            .filter(|s| {
                (ids.is_empty() && tags.is_empty())
                    || ids.contains(&s.id)
                    || s.tags.iter().any(|t| tags.contains(t))
            })
            .collect()
    }
}
