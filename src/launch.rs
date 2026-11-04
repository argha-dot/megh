//! Per-game Wine prefixes and launching.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::data_dir;
use crate::runtime::{self, Arch, Graphics, Runtime};

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

/// Copies the layer's DLLs over Wine's own in the prefix.
/// - 32-bit DLLs go in syswow64, 64-bit ones in system32.
/// - Runs every launch: cheap, and redoes it if Wine puts its own copies back.
fn install_dlls(gfx: &Graphics, arch: Arch, prefix: &Path) -> Result<()> {
    let (from, to) = match arch {
        Arch::I386 => ("x32", "syswow64"),
        Arch::X86_64 => ("x64", "system32"),
    };
    let to = prefix.join("drive_c/windows").join(to);
    for dll in &gfx.dlls {
        let name = format!("{dll}.dll");
        let src = gfx.root.join(from).join(&name);
        let bytes = fs::read(&src).with_context(|| format!("{} not found", src.display()))?;
        let dst = to.join(&name);
        if fs::read(&dst).ok().as_ref() != Some(&bytes) {
            // Remove first, so a symlinked DLL never gets written through.
            if dst.symlink_metadata().is_ok() {
                fs::remove_file(&dst)?;
            }
            fs::write(&dst, &bytes)?;
        }
    }
    Ok(())
}

/// megh's env for a graphics layer: its DLLs native-first, then its env.
fn graphics_env(gfx: &Graphics) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([("WINEDLLOVERRIDES".to_string(), format!("{}=n,b", gfx.dlls.join(",")))]);
    env.extend(gfx.env.clone());
    env
}

/// `env` is applied last, so per-game values override megh's defaults.
#[allow(clippy::too_many_arguments)]
pub fn run(
    rt: &Runtime,
    gfx: Option<&Graphics>,
    exe: &Path,
    prefix: &Path,
    hud: bool,
    log: bool,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<()> {
    // Wine fails on bare relative names, so always pass an absolute path.
    let exe = exe
        .canonicalize()
        .with_context(|| format!("{} not found", exe.display()))?;
    let game_dir = exe.parent().context("exe has no parent folder")?;
    ensure_prefix(rt, prefix)?;

    let mut cmd = wine(rt, prefix);
    cmd.arg(&exe).args(args).current_dir(game_dir);
    if let Some(gfx) = gfx {
        install_dlls(gfx, runtime::exe_arch(&exe)?, prefix)?;
        cmd.envs(graphics_env(gfx));
    }
    if hud {
        // Still set it: a per-game Vulkan/MoltenVK setup can end up on Metal too.
        cmd.env("MTL_HUD_ENABLED", "1");
        // Graphics layers (DXVK) draw through MoltenVK, so the HUD shows.
        if !rt.metal_hud && gfx.is_none() {
            eprintln!(
                "Note: '{}' doesn't draw through Metal directly, so the HUD probably won't show.",
                rt.id
            );
        }
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
    cmd.envs(env);

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

    /// A fake layer with x32/x64 DLLs, and a prefix with Wine's own copies.
    fn setup(tmp: &Path) -> (Graphics, PathBuf) {
        let root = tmp.join("dxvk");
        for (dir, text) in [("x32", "dxvk32"), ("x64", "dxvk64")] {
            fs::create_dir_all(root.join(dir)).unwrap();
            fs::write(root.join(dir).join("d3d11.dll"), text).unwrap();
        }
        let prefix = tmp.join("prefix");
        for dir in ["syswow64", "system32"] {
            let sys = prefix.join("drive_c/windows").join(dir);
            fs::create_dir_all(&sys).unwrap();
            fs::write(sys.join("d3d11.dll"), "wine").unwrap();
        }
        let gfx = Graphics { root, dlls: vec!["d3d11".into()], env: BTreeMap::new() };
        (gfx, prefix)
    }

    fn read(prefix: &Path, dir: &str) -> String {
        fs::read_to_string(prefix.join("drive_c/windows").join(dir).join("d3d11.dll")).unwrap()
    }

    #[test]
    fn dlls_go_to_the_folder_for_the_exe_bitness() {
        let tmp = tempfile::tempdir().unwrap();
        let (gfx, prefix) = setup(tmp.path());
        install_dlls(&gfx, Arch::I386, &prefix).unwrap();
        assert_eq!(read(&prefix, "syswow64"), "dxvk32");
        assert_eq!(read(&prefix, "system32"), "wine");
        install_dlls(&gfx, Arch::X86_64, &prefix).unwrap();
        assert_eq!(read(&prefix, "system32"), "dxvk64");
    }

    #[test]
    fn identical_dlls_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (gfx, prefix) = setup(tmp.path());
        install_dlls(&gfx, Arch::I386, &prefix).unwrap();
        let dst = prefix.join("drive_c/windows/syswow64/d3d11.dll");
        File::options().write(true).open(&dst).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
        install_dlls(&gfx, Arch::I386, &prefix).unwrap();
        assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), std::time::UNIX_EPOCH);
    }

    #[test]
    fn symlinked_dlls_are_replaced_not_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let (gfx, prefix) = setup(tmp.path());
        // Stand-in for a DLL inside the runtime itself.
        let runtime_dll = tmp.path().join("runtime-d3d11.dll");
        fs::write(&runtime_dll, "wine").unwrap();
        let dst = prefix.join("drive_c/windows/syswow64/d3d11.dll");
        fs::remove_file(&dst).unwrap();
        symlink(&runtime_dll, &dst).unwrap();

        install_dlls(&gfx, Arch::I386, &prefix).unwrap();
        assert!(!dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(read(&prefix, "syswow64"), "dxvk32");
        assert_eq!(fs::read_to_string(&runtime_dll).unwrap(), "wine");
    }

    #[test]
    fn graphics_env_overrides_dlls_and_adds_layer_env() {
        let gfx = Graphics {
            root: PathBuf::new(),
            dlls: vec!["d3d11".into(), "d3d10core".into()],
            env: BTreeMap::from([("A".into(), "1".into())]),
        };
        let env = graphics_env(&gfx);
        assert_eq!(env["WINEDLLOVERRIDES"], "d3d11,d3d10core=n,b");
        assert_eq!(env["A"], "1");
        // The layer's env can replace megh's override.
        let custom = Graphics { env: BTreeMap::from([("WINEDLLOVERRIDES".into(), "x=n".into())]), ..gfx };
        assert_eq!(graphics_env(&custom)["WINEDLLOVERRIDES"], "x=n");
    }
}
