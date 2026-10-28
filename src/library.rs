//! Installed games, stored in <data_dir>/library.yml.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::data_dir;

#[derive(Serialize, Deserialize, Default)]
pub struct Library {
    #[serde(default)]
    pub games: BTreeMap<String, Game>,
}

#[derive(Serialize, Deserialize)]
pub struct Game {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gog_id: Option<u64>,
    pub install_dir: PathBuf,
    /// Relative to install_dir.
    pub exe: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Runtime id from runtimes.yml; auto-picked by exe bitness if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Ludusavi-style path templates, e.g. "<home>/AppData/LocalLow/Foo".
    #[serde(default)]
    pub saves: Vec<String>,
}

impl Library {
    fn path() -> Result<PathBuf> {
        Ok(data_dir()?.join("library.yml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        serde_saphyr::from_str(&text).with_context(|| format!("bad {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(data_dir()?)?;
        fs::write(Self::path()?, serde_saphyr::to_string(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_yaml() {
        let mut library = Library::default();
        library.games.insert(
            "forager".into(),
            Game {
                name: "Forager".into(),
                gog_id: Some(2106942030),
                install_dir: PathBuf::from("/data/megh/games/forager"),
                exe: PathBuf::from("Forager.exe"),
                args: vec!["-windowed".into()],
                runtime: Some("wine-crossover-23.7.1-1".into()),
                saves: vec!["<winLocalAppData>/Forager".into()],
            },
        );
        let yaml = serde_saphyr::to_string(&library).unwrap();
        let back: Library = serde_saphyr::from_str(&yaml).unwrap();
        let game = &back.games["forager"];
        assert_eq!(game.gog_id, Some(2106942030));
        assert_eq!(game.args, ["-windowed"]);
        assert_eq!(game.runtime.as_deref(), Some("wine-crossover-23.7.1-1"));
        assert_eq!(game.saves, ["<winLocalAppData>/Forager"]);
    }

    #[test]
    fn loads_entries_from_before_runtimes_existed() {
        // Written by an older megh: no runtime, gog_id, args or saves.
        let yaml = "games:\n  shogun-showdown:\n    name: Shogun Showdown\n    install_dir: /x\n    exe: ShogunShowdown.exe\n";
        let library: Library = serde_saphyr::from_str(yaml).unwrap();
        let game = &library.games["shogun-showdown"];
        assert!(game.runtime.is_none() && game.gog_id.is_none());
        assert!(game.args.is_empty() && game.saves.is_empty());
    }
}
