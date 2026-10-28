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
