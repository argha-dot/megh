//! Per-game Wine prefixes and launching.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::data_dir;
use crate::runtime::Runtime;

pub fn prefix_path(name: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("prefixes").join(name))
}

/// Creates the prefix if needed and keeps it isolated from the Mac's user folders.
pub fn ensure_prefix(rt: &Runtime, prefix: &Path) -> Result<()> {
    if !prefix.join("drive_c").is_dir() {
        // Wine creates the prefix itself, but not its parent folders.
        fs::create_dir_all(prefix.parent().unwrap())?;
        println!("Creating Wine prefix at {} ...", prefix.display());
        let status = wine(rt, prefix)
            .arg("wineboot")
            .arg("--init")
            .env("WINEDEBUG", "-all")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("wineboot failed");
        }
    }
    unlink_user_folders(prefix)
}

/// The prefix's Windows user folder (CrossOver Wine names it "crossover").
pub fn user_dir(prefix: &Path) -> Result<PathBuf> {
    let users = prefix.join("drive_c/users");
    for entry in fs::read_dir(&users)? {
        let entry = entry?;
        if entry.file_name() != "Public" && entry.path().is_dir() {
            return Ok(entry.path());
        }
    }
    bail!("no user folder in {}", users.display())
}

/// Wine links Documents, Downloads, etc. to the real ~/ folders.
/// - Replace them with plain folders so saves stay in the prefix.
/// - Checked on every launch, in case Wine recreates them.
fn unlink_user_folders(prefix: &Path) -> Result<()> {
    for entry in fs::read_dir(user_dir(prefix)?)? {
        let path = entry?.path();
        if path.symlink_metadata()?.file_type().is_symlink() {
            fs::remove_file(&path)?;
            fs::create_dir(&path)?;
        }
    }
    Ok(())
}

pub fn run(
    rt: &Runtime,
    exe: &Path,
    prefix: &Path,
    hud: bool,
    log: bool,
    args: &[String],
) -> Result<()> {
    // Wine fails on bare relative names, so always pass an absolute path.
    let exe = exe
        .canonicalize()
        .with_context(|| format!("{} not found", exe.display()))?;
    let game_dir = exe.parent().context("exe has no parent folder")?;
    ensure_prefix(rt, prefix)?;

    let mut cmd = wine(rt, prefix);
    cmd.arg(&exe).args(args).current_dir(game_dir);
    if hud {
        cmd.env("MTL_HUD_ENABLED", "1");
    }
    if log {
        let logs = data_dir()?.join("logs");
        fs::create_dir_all(&logs)?;
        let name = prefix.file_name().unwrap_or_default().to_string_lossy();
        let path = logs.join(format!("{name}.log"));
        let file = File::create(&path)?;
        cmd.stdout(file.try_clone()?).stderr(file);
        println!("Logging to {}", path.display());
    } else {
        cmd.env("WINEDEBUG", "-all");
    }

    println!("Launching {} ...", exe.display());
    let status = cmd.status()?;
    if !status.success() {
        bail!("game exited with {status}");
    }
    Ok(())
}

/// Every wine call must share these env vars.
/// - A wineserver left running from wineboot with different sync
///   settings makes the game fail to start (esync_init error).
pub fn wine(rt: &Runtime, prefix: &Path) -> Command {
    let mut cmd = Command::new(rt.wine());
    cmd.env("WINEPREFIX", prefix)
        .env("WINEMSYNC", "1")
        .env("WINEESYNC", "1");
    cmd
}

/// Blocks until the prefix's wineserver has exited.
pub fn wine_server_wait(rt: &Runtime, prefix: &Path) -> Result<()> {
    let wineserver = rt.wine().with_file_name("wineserver");
    Command::new(wineserver).arg("-w").env("WINEPREFIX", prefix).status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn user_dir_skips_public() {
        let tmp = tempfile::tempdir().unwrap();
        let users = tmp.path().join("drive_c/users");
        fs::create_dir_all(users.join("Public")).unwrap();
        fs::create_dir_all(users.join("crossover")).unwrap();
        assert_eq!(user_dir(tmp.path()).unwrap(), users.join("crossover"));

        let empty = tempfile::tempdir().unwrap();
        fs::create_dir_all(empty.path().join("drive_c/users/Public")).unwrap();
        assert!(user_dir(empty.path()).is_err());
    }

    #[test]
    fn unlinking_keeps_the_real_folders_intact() {
        let tmp = tempfile::tempdir().unwrap();
        // Stand-in for the real ~/Documents.
        let real_docs = tmp.path().join("mac-home/Documents");
        fs::create_dir_all(&real_docs).unwrap();
        fs::write(real_docs.join("taxes.pdf"), "important").unwrap();

        let prefix = tmp.path().join("prefix");
        let user = prefix.join("drive_c/users/crossover");
        fs::create_dir_all(user.join("AppData")).unwrap();
        fs::write(user.join("AppData/keep.txt"), "x").unwrap();
        symlink(&real_docs, user.join("Documents")).unwrap();

        unlink_user_folders(&prefix).unwrap();

        let docs = user.join("Documents");
        assert!(!docs.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(docs.is_dir());
        assert_eq!(fs::read_dir(&docs).unwrap().count(), 0);
        // The Mac folder and the prefix's own folders are untouched.
        assert_eq!(fs::read_to_string(real_docs.join("taxes.pdf")).unwrap(), "important");
        assert!(user.join("AppData/keep.txt").is_file());
    }
}
