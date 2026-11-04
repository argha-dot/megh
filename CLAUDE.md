# CLAUDE.md

Megh: a Rust CLI to install and run Windows games on Apple Silicon Macs with Wine.
Inspired by GameNative (Android). GPL-3.0-only. The README is the owner's; don't rewrite it unasked.

## Commands
- `cargo run -- <command>`: megh isn't installed on PATH; always run it this way.
- `cargo test`: unit tests, no Wine or network needed.
- `cargo clippy --all-targets -- -D warnings`: what CI runs (`.github/workflows/ci.yml`, macOS arm64).
- Gotcha: `cargo test` and `cargo clippy` don't rebuild `target/debug/megh`.
  - Use `cargo build` / `cargo run` before running the binary directly.
- Code isn't `rustfmt`-formatted; don't run `cargo fmt` (it would reformat everything).

## Layout
- `src/main.rs`: clap CLI, `data_dir()`, `confirm()` prompts.
- `src/runtime.rs`: Wine runtimes from `runtimes.yml`; auto-pick by exe bitness (`exe_arch`, `pick`); install by path or download.
- `src/install.rs`: `install` (GOG offline installers), `add` (games already on disk), `delete`; both share `register`.
- `src/launch.rs`: per-game prefixes and launching; `wine()` builds every Wine command.
- `src/saves.rs`: save paths from the Ludusavi manifest, backup and restore.
- `src/library.rs`: `library.yml` (installed games).
- `runtimes.yml`: built-in runtime and graphics lists, compiled in via `include_str!`.
- `tests/fixtures/`: test data (a real `goggame-*.info`).

## Data dir (`~/Library/Application Support/megh/`)
- `library.yml`: games; hand-editable (`args`, `runtime`, `graphics`, `env`, `saves`).
- `runtimes.yml`: user additions/overrides to the built-in list, matched by `id`.
- `runtimes/<id>/`: extracted runtimes, or a symlink to an existing folder.
- `graphics/<id>/`: extracted graphics layers (DXVK), same install flow as runtimes.
- `games/<slug>/`: games megh installed. Added games live elsewhere and are never deleted.
- `prefixes/<slug>/`: one Wine prefix per game.
- `saves/<slug>/<timestamp>/`: backups (keep 1), mirrored as `prefix/...` and `game/...`.
- `cache/ludusavi-manifest.yaml`: re-downloaded when over 7 days old.

## Wine / macOS facts (each cost real debugging)
- Wine needs an absolute exe path; bare relative names fail with "Environment variable not found".
- Wine won't create a prefix's parent folders.
- Every Wine call must share the same sync env (`WINEMSYNC`/`WINEESYNC`), or the game fails with an esync error.
  - That's why everything goes through `launch::wine()`.
- The prefix's Windows user folder is `crossover` on GPTK, but the Mac username on Wine Crossover.
  - Never hardcode it; use `launch::user_dir()`. Restore remaps it.
- Wine links Documents/Downloads/etc. to the real `~/` folders; megh replaces those links with plain folders.
- Runtimes:
  - GPTK 3.0-3 (Wine 7.7 + D3DMetal): 64-bit only. 32-bit games crash in its old 32on64 mode.
  - Wine Crossover 23.7.1 (Wine 8): 32-bit games. DX goes through WineD3D on OpenGL.
- The Metal HUD (`--hud`) only appears with D3DMetal (`metal_hud: true` in `runtimes.yml`). OpenGL-based games never show it.
- 32-bit Unity DX11 games fail on Wine Crossover ("d3d11: failed to create device"). DXVK fixes it (`graphics:` in `runtimes.yml`):
  - DXVK-macOS has only d3d11/d3d10core (no dxgi, no d3d9).
  - Its non-builtin DLLs go in the prefix (x32 → `syswow64`, x64 → `system32`) with `WINEDLLOVERRIDES=d3d11,d3d10core=n,b`.
  - `WINEDLLPATH` with the builtin archive is ignored by Wine.
  - `MVK_CONFIG_FAST_MATH_ENABLED=0`, or bloom blows out to white.
  - Turning it off is just removing `graphics:`: without the override, Wine loads its builtin DLLs and ignores the copies.
- `Read access denied for device Z:\` warnings are harmless.

## Conventions
- Tests never touch `$HOME` or the network.
  - Core logic takes paths/data as arguments (`backup_into`, `find_saves`, `merge`, `pick`); thin wrappers add `data_dir()`.
  - Use `tempfile` for filesystem tests.
- Shell out to macOS tools (`curl`, `tar`, `ditto`, `shasum`, `date`) rather than adding crates for them.
- Downloads are checked against the sha256 in `runtimes.yml`.
- serde-saphyr: the Ludusavi manifest needs `options.budget = None`. The default limits reject it.
- `owned_by_megh` guards deletes: only inside `games/`, never with `..`.
