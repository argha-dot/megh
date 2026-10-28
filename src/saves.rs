//! Save locations (from the Ludusavi manifest) and local backups.
//! - Steam Cloud and GOG Galaxy keep only the latest save.
//! - We keep the last KEEP_BACKUPS copies, taken before each launch and delete.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::data_dir;
use crate::launch;
use crate::library::Game;

const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml";
const MANIFEST_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const KEEP_BACKUPS: usize = 1;

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
/// - Re-downloads the manifest when it's over a week old.
/// - Falls back to the cached copy if that fails (e.g. offline).
pub fn lookup(gog_id: Option<u64>, name: &str) -> Result<Vec<String>> {
    let path = data_dir()?.join("cache/ludusavi-manifest.yaml");
    let modified = fs::metadata(&path).and_then(|m| m.modified());
    let fresh = modified.is_ok_and(|t| !is_stale(t, SystemTime::now()));
    if !fresh && let Err(e) = download_manifest(&path) {
        if !path.exists() {
            return Err(e);
        }
        eprintln!("Warning: {e:#}; using the cached save manifest");
    }
    find_saves(&fs::read_to_string(&path)?, gog_id, name)
}

fn is_stale(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified).is_ok_and(|age| age > MANIFEST_MAX_AGE)
}

/// Downloads to a .part file first so a failed download keeps the old copy.
fn download_manifest(path: &Path) -> Result<()> {
    println!("Downloading Ludusavi save manifest (~18MB) ...");
    fs::create_dir_all(path.parent().unwrap())?;
    let part = path.with_extension("yaml.part");
    let status = Command::new("curl")
        .args(["-L", "--fail", "-s", "-o"])
        .arg(&part)
        .arg(MANIFEST_URL)
        .status()?;
    if !status.success() {
        let _ = fs::remove_file(&part);
        bail!("failed to download {MANIFEST_URL}");
    }
    fs::rename(&part, path)?;
    Ok(())
}

fn find_saves(manifest: &str, gog_id: Option<u64>, name: &str) -> Result<Vec<String>> {
    // The manifest is far bigger than the parser's default safety limits.
    let mut options = serde_saphyr::Options::default();
    options.budget = None;
    let manifest: BTreeMap<String, Entry> =
        serde_saphyr::from_str_with_options(manifest, options).context("bad Ludusavi manifest")?;
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
    if let Some(dest) = backup_into(&backups_dir(slug)?, game, prefix)? {
        println!("Backed up saves to {}", dest.display());
    }
    Ok(())
}

/// Returns the new backup folder, or None if there was nothing to save.
fn backup_into(dir: &Path, game: &Game, prefix: &Path) -> Result<Option<PathBuf>> {
    let mut found = Vec::new();
    for template in &game.saves {
        found.extend(resolve(template, game, prefix)?);
    }
    if found.is_empty() {
        return Ok(None);
    }

    let out = Command::new("date").arg("+%Y-%m-%d_%H-%M-%S").output()?;
    let stamp = String::from_utf8(out.stdout)?.trim().to_string();
    let dest = dir.join(&stamp);
    for src in &found {
        // Mirror the layout so a restore knows where each file goes.
        let rel = match (src.strip_prefix(prefix), src.strip_prefix(&game.install_dir)) {
            (Ok(rel), _) => Path::new("prefix").join(rel),
            (_, Ok(rel)) => Path::new("game").join(rel),
            _ => continue,
        };
        ditto(src, &dest.join(rel))?;
    }

    let backups = list_backups(dir)?;
    for old in backups.iter().take(backups.len().saturating_sub(KEEP_BACKUPS)) {
        fs::remove_dir_all(dir.join(old))?;
    }
    Ok(Some(dest))
}

pub fn latest_backup(slug: &str) -> Result<Option<PathBuf>> {
    let dir = backups_dir(slug)?;
    Ok(list_backups(&dir)?.pop().map(|name| dir.join(name)))
}

/// Copies a backup back into the prefix and game folder, merging over what's there.
/// - The Windows user folder name differs per runtime (crossover, the Mac username, ...),
///   so any backed-up user folder goes into the prefix's current one.
pub fn restore(backup: &Path, game: &Game, prefix: &Path) -> Result<()> {
    let user = launch::user_dir(prefix)?;
    for entry in entries(&backup.join("prefix/drive_c/users"))? {
        let to = if entry.file_name() == "Public" {
            prefix.join("drive_c/users/Public")
        } else {
            user.clone()
        };
        ditto(&entry.path(), &to)?;
    }
    // Anything else in the prefix keeps its path.
    let rest = [
        (backup.join("prefix"), prefix.to_path_buf(), "drive_c"),
        (backup.join("prefix/drive_c"), prefix.join("drive_c"), "users"),
    ];
    for (from, to, skip) in rest {
        for entry in entries(&from)? {
            if entry.file_name() != skip {
                ditto(&entry.path(), &to.join(entry.file_name()))?;
            }
        }
    }
    let saved_game = backup.join("game");
    if saved_game.exists() {
        ditto(&saved_game, &game.install_dir)?;
    }
    Ok(())
}

fn entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    Ok(fs::read_dir(dir)?.collect::<Result<_, _>>()?)
}

/// Copies a file or folder, merging into an existing folder.
fn ditto(from: &Path, to: &Path) -> Result<()> {
    let status = Command::new("ditto").arg(from).arg(to).status()?;
    if !status.success() {
        bail!("failed to copy {} to {}", from.display(), to.display());
    }
    Ok(())
}

/// Backup folder names in `dir`, oldest first.
fn list_backups(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = fs::read_dir(dir)?
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

    let dir = backups_dir(slug)?;
    let backups = list_backups(&dir)?;
    println!("\nBackups (last {KEEP_BACKUPS}) in {}:", dir.display());
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

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
Shogun Showdown:
  files:
    "<home>/AppData/LocalLow/Roboatino/ShogunShowdown":
      tags: [save]
      when:
        - os: windows
    "<base>/settings.cfg":
      tags: [config]
    "<home>/Library/Shogun":
      tags: [save]
      when:
        - os: mac
  gog:
    id: 1104084973
Steamy:
  files:
    "<winDocuments>/My Games/Steamy":
      tags: [save]
      when:
        - store: steam
    "<winAppData>/Steamy":
      tags: [save]
Empty:
  gog:
    id: 1
"#;

    /// A prefix with a Windows user folder named `user`.
    fn fake_prefix(root: &Path, user: &str) -> PathBuf {
        let prefix = root.join(format!("prefix-{user}"));
        fs::create_dir_all(prefix.join("drive_c/users").join(user)).unwrap();
        fs::create_dir_all(prefix.join("drive_c/users/Public")).unwrap();
        prefix
    }

    fn game(install_dir: PathBuf, saves: &[&str]) -> Game {
        Game {
            name: "Test".into(),
            gog_id: None,
            install_dir,
            exe: PathBuf::from("Game.exe"),
            args: Vec::new(),
            runtime: None,
            saves: saves.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn wanted_keeps_windows_and_gog_saves_only() {
        let rule = |tags: &[&str], when: Vec<When>| FileRule {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            when,
        };
        let when = |os: Option<&str>, store: Option<&str>| When {
            os: os.map(Into::into),
            store: store.map(Into::into),
        };
        assert!(rule(&["save"], vec![]).wanted());
        assert!(rule(&["save"], vec![when(Some("windows"), None)]).wanted());
        assert!(rule(&["save"], vec![when(None, Some("gog"))]).wanted());
        assert!(!rule(&["config"], vec![]).wanted());
        assert!(!rule(&["save"], vec![when(Some("mac"), None)]).wanted());
        assert!(!rule(&["save"], vec![when(None, Some("steam"))]).wanted());
        // Any one matching condition is enough.
        assert!(rule(&["save"], vec![when(Some("linux"), None), when(Some("windows"), None)]).wanted());
    }

    #[test]
    fn find_saves_by_gog_id_then_name() {
        let shogun = vec!["<home>/AppData/LocalLow/Roboatino/ShogunShowdown".to_string()];
        assert_eq!(find_saves(MANIFEST, Some(1104084973), "whatever").unwrap(), shogun);
        assert_eq!(find_saves(MANIFEST, Some(999), "Shogun Showdown").unwrap(), shogun);
        assert_eq!(find_saves(MANIFEST, None, "Steamy").unwrap(), ["<winAppData>/Steamy"]);
        assert!(find_saves(MANIFEST, Some(1), "Empty").unwrap().is_empty());
        assert!(find_saves(MANIFEST, None, "Unknown").unwrap().is_empty());
    }

    #[test]
    fn resolve_handles_spaces_and_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        // Like ~/Library/Application Support/megh.
        let root = tmp.path().join("Application Support/megh [x]");
        let prefix = fake_prefix(&root, "crossover");
        let saves = prefix.join("drive_c/users/crossover/AppData/LocalLow/Roboatino/ShogunShowdown");
        fs::create_dir_all(&saves).unwrap();
        let g = game(root.join("games/shogun"), &[]);

        let found = resolve("<home>/AppData/LocalLow/Roboatino/ShogunShowdown", &g, &prefix).unwrap();
        assert_eq!(found, std::slice::from_ref(&saves));
        let found = resolve("<winLocalAppDataLow>/Roboatino/*", &g, &prefix).unwrap();
        assert_eq!(found, [saves]);
        // Not created yet, or Linux-only.
        assert!(resolve("<winAppData>/Nope", &g, &prefix).unwrap().is_empty());
        assert!(resolve("<xdgData>/shogun", &g, &prefix).unwrap().is_empty());
    }

    #[test]
    fn backup_then_restore_into_a_differently_named_user() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("games/forager");
        let g = game(install.clone(), &["<winLocalAppData>/Forager", "<base>/saves"]);

        // Saved under Wine Crossover, whose user is named after the Mac account.
        let old = fake_prefix(tmp.path(), "alice");
        write(&old.join("drive_c/users/alice/AppData/Local/Forager/saveData0/Data.cin"), "progress");
        write(&install.join("saves/slot1.sav"), "slot");

        let backups = tmp.path().join("backups");
        let dest = backup_into(&backups, &g, &old).unwrap().unwrap();
        assert!(dest.join("prefix/drive_c/users/alice/AppData/Local/Forager/saveData0/Data.cin").is_file());

        // Restore into a GPTK prefix (user "crossover") after a reinstall.
        fs::remove_dir_all(install.join("saves")).unwrap();
        let new = fake_prefix(tmp.path(), "crossover");
        restore(&dest, &g, &new).unwrap();

        let data = new.join("drive_c/users/crossover/AppData/Local/Forager/saveData0/Data.cin");
        assert_eq!(fs::read_to_string(data).unwrap(), "progress");
        assert!(!new.join("drive_c/users/alice").exists());
        assert_eq!(fs::read_to_string(install.join("saves/slot1.sav")).unwrap(), "slot");
    }

    #[test]
    fn backup_keeps_only_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = fake_prefix(tmp.path(), "crossover");
        write(&prefix.join("drive_c/users/crossover/AppData/Roaming/X/save"), "1");
        let g = game(tmp.path().join("games/x"), &["<winAppData>/X"]);

        let backups = tmp.path().join("backups");
        fs::create_dir_all(backups.join("2000-01-01_00-00-00")).unwrap();
        let dest = backup_into(&backups, &g, &prefix).unwrap().unwrap();

        let left = list_backups(&backups).unwrap();
        assert_eq!(left.len(), KEEP_BACKUPS);
        assert_eq!(backups.join(left.last().unwrap()), dest);
    }

    #[test]
    fn backup_with_no_saves_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = fake_prefix(tmp.path(), "crossover");
        let g = game(tmp.path().join("games/x"), &["<winAppData>/X"]);
        let backups = tmp.path().join("backups");
        assert!(backup_into(&backups, &g, &prefix).unwrap().is_none());
        assert!(!backups.exists());
    }

    #[test]
    fn manifest_goes_stale_after_a_week() {
        let now = SystemTime::now();
        let days = |n: u64| Duration::from_secs(n * 24 * 60 * 60);
        assert!(!is_stale(now, now));
        assert!(!is_stale(now - days(6), now));
        assert!(is_stale(now - days(8), now));
        // Clock skew: a future timestamp isn't stale.
        assert!(!is_stale(now + days(1), now));
    }

    #[test]
    fn list_backups_is_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["2026-10-28_23-41-31", "2025-01-01_00-00-00", "2026-01-15_12-00-00"] {
            fs::create_dir_all(tmp.path().join(name)).unwrap();
        }
        fs::write(tmp.path().join("stray-file"), "").unwrap();
        assert_eq!(
            list_backups(tmp.path()).unwrap(),
            ["2025-01-01_00-00-00", "2026-01-15_12-00-00", "2026-10-28_23-41-31"]
        );
        assert!(list_backups(&tmp.path().join("missing")).unwrap().is_empty());
    }
}
