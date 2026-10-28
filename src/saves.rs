//! Save locations (from the Ludusavi manifest) and local backups.
//! - Steam Cloud and GOG Galaxy keep only the latest save.
//! - We keep the last KEEP_BACKUPS copies, taken before each launch.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::data_dir;
use crate::launch;
use crate::library::Game;

const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml";
const KEEP_BACKUPS: usize = 5;

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    files: BTreeMap<String, FileRule>,
    gog: Option<GogRef>,
}

#[derive(Deserialize)]
struct GogRef {
    id: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileRule {
    tags: Vec<String>,
    when: Vec<When>,
}

#[derive(Deserialize)]
struct When {
    os: Option<String>,
    store: Option<String>,
}

impl FileRule {
    /// Windows saves, for any store or for GOG.
    fn wanted(&self) -> bool {
        let windows = |w: &When| {
            w.os.as_deref().is_none_or(|os| os == "windows")
                && w.store.as_deref().is_none_or(|s| s == "gog")
        };
        self.tags.iter().any(|t| t == "save")
            && (self.when.is_empty() || self.when.iter().any(windows))
    }
}

/// Save path templates for a game, by GOG id or exact name.
pub fn lookup(gog_id: Option<u64>, name: &str) -> Result<Vec<String>> {
    let path = data_dir()?.join("cache/ludusavi-manifest.yaml");
    if !path.exists() {
        println!("Downloading Ludusavi save manifest (~18MB) ...");
        fs::create_dir_all(path.parent().unwrap())?;
        let status = Command::new("curl")
            .args(["-L", "--fail", "-s", "-o"])
            .arg(&path)
            .arg(MANIFEST_URL)
            .status()?;
        if !status.success() {
            bail!("failed to download {MANIFEST_URL}");
        }
    }

    let text = fs::read_to_string(&path)?;
    // The manifest is far bigger than the parser's default safety limits.
    let mut options = serde_saphyr::Options::default();
    options.budget = None;
    let manifest: BTreeMap<String, Entry> =
        serde_saphyr::from_str_with_options(&text, options).context("bad Ludusavi manifest")?;
    let by_id = gog_id.and_then(|id| {
        manifest
            .values()
            .find(|e| e.gog.as_ref().and_then(|g| g.id) == Some(id))
    });
    let Some(entry) = by_id.or_else(|| manifest.get(name)) else {
        return Ok(Vec::new());
    };
    Ok(entry
        .files
        .iter()
        .filter(|(_, rule)| rule.wanted())
        .map(|(path, _)| path.clone())
        .collect())
}

/// Existing paths matching a template, inside the game's prefix.
fn resolve(template: &str, game: &Game, prefix: &Path) -> Result<Vec<PathBuf>> {
    let user = launch::user_dir(prefix)?;
    let drive_c = prefix.join("drive_c");
    let esc = |p: &Path| glob::Pattern::escape(&p.to_string_lossy());
    let user_name = user.file_name().unwrap_or_default().to_string_lossy();

    let vars = [
        ("<base>", esc(&game.install_dir)),
        ("<root>", esc(game.install_dir.parent().unwrap_or(&game.install_dir))),
        ("<game>", glob::Pattern::escape(&game.install_dir.file_name().unwrap_or_default().to_string_lossy())),
        ("<home>", esc(&user)),
        ("<osUserName>", glob::Pattern::escape(&user_name)),
        ("<storeUserId>", "*".to_string()),
        ("<winAppData>", esc(&user.join("AppData/Roaming"))),
        ("<winLocalAppData>", esc(&user.join("AppData/Local"))),
        ("<winLocalAppDataLow>", esc(&user.join("AppData/LocalLow"))),
        ("<winDocuments>", esc(&user.join("Documents"))),
        ("<winPublic>", esc(&drive_c.join("users/Public"))),
        ("<winProgramData>", esc(&drive_c.join("ProgramData"))),
        ("<winDir>", esc(&drive_c.join("windows"))),
    ];
    let mut pattern = template.to_string();
    for (key, value) in &vars {
        pattern = pattern.replace(key, value);
    }
    // Linux/Mac-only placeholders (<xdgData> etc.) don't apply.
    if pattern.contains('<') {
        return Ok(Vec::new());
    }
    Ok(glob::glob(&pattern)?.filter_map(Result::ok).collect())
}

fn backups_dir(slug: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("saves").join(slug))
}

/// Copies existing saves to saves/<slug>/<timestamp>/, keeping the newest few.
pub fn backup(slug: &str, game: &Game, prefix: &Path) -> Result<()> {
    let mut found = Vec::new();
    for template in &game.saves {
        found.extend(resolve(template, game, prefix)?);
    }
    if found.is_empty() {
        return Ok(());
    }

    let out = Command::new("date").arg("+%Y-%m-%d_%H-%M-%S").output()?;
    let stamp = String::from_utf8(out.stdout)?.trim().to_string();
    let dest = backups_dir(slug)?.join(&stamp);
    for src in &found {
        // Mirror the layout so a restore knows where each file goes.
        let rel = match (src.strip_prefix(prefix), src.strip_prefix(&game.install_dir)) {
            (Ok(rel), _) => Path::new("prefix").join(rel),
            (_, Ok(rel)) => Path::new("game").join(rel),
            _ => continue,
        };
        let status = Command::new("ditto").arg(src).arg(dest.join(rel)).status()?;
        if !status.success() {
            bail!("failed to back up {}", src.display());
        }
    }
    println!("Backed up saves to {}", dest.display());

    let backups = list_backups(slug)?;
    for old in backups.iter().take(backups.len().saturating_sub(KEEP_BACKUPS)) {
        fs::remove_dir_all(backups_dir(slug)?.join(old))?;
    }
    Ok(())
}

/// Backup folder names, oldest first.
fn list_backups(slug: &str) -> Result<Vec<String>> {
    let dir = backups_dir(slug)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// `megh saves <slug>`
pub fn show(slug: &str, game: &Game, prefix: &Path) -> Result<()> {
    println!("Save locations for {}:", game.name);
    if game.saves.is_empty() {
        println!("  none known (not in the Ludusavi manifest)");
    }
    for template in &game.saves {
        println!("  {template}");
        let found = resolve(template, game, prefix)?;
        if found.is_empty() {
            println!("    (nothing yet)");
        }
        for path in found {
            println!("    {} ({} files)", path.display(), count_files(&path));
        }
    }

    let backups = list_backups(slug)?;
    println!("\nBackups (last {KEEP_BACKUPS}) in {}:", backups_dir(slug)?.display());
    if backups.is_empty() {
        println!("  none yet");
    }
    for name in backups.iter().rev() {
        println!("  {name}");
    }
    Ok(())
}

fn count_files(path: &Path) -> usize {
    if !path.is_dir() {
        return 1;
    }
    fs::read_dir(path)
        .map(|entries| entries.filter_map(Result::ok).map(|e| count_files(&e.path())).sum())
        .unwrap_or(0)
}
