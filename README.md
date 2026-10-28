# Megh

## What it is
A nascent project to run Windows games on macOS. Very early and experimental. Not maintained. Currently a CLI, but I am planning on slapping a GUI on it. This is vibe-coded, and, breaking no new stereotypes, we're using Rust.

## Requirements
 - Apple Silicon Mac with macOS 14+
 - Rosetta 2 (`softwareupdate --install-rosetta`)
 - Rust, 'cause there are no releases available yet
 - Optional: Homebrew with `innoextract` (`brew install innoextract`)

## Build and Run
`cargo run -- <command>`

## Quick Start
At the time of writing there's no cloud support. Saves stay local, and game installation folders are needed. So, ideally, you should be using a DRM-free offline installer, like the ones GOG provides.

## Commands
`cargo run -- -h`

## Tested Games
 - Forager
 - Shogun Showdown

## Future Plans
 - Add documentation, tests and other basic/necessary project setup things
   - [x] Basic tests
   - [x] Add README
   - [x] Decide on a license
   - [ ] Docs
 - Get Steam / GOG support working in here.
 - UI to do things.
 - A hundred other things

## Credits
Megh stands on other people's work. It downloads and runs these, but doesn't bundle them:
 - [Wine](https://www.winehq.org) Is Not an Emulator.
 - [Gcenx](https://github.com/Gcenx)'s macOS Wine builds, including the [Game Porting Toolkit](https://github.com/Gcenx/game-porting-toolkit) build. It contains Apple's D3DMetal, which is under [Apple's own license](https://developer.apple.com/games/); by downloading it, you're agreeing to that.
 - [Heroic Games Launcher](https://github.com/Heroic-Games-Launcher)'s [wine-crossover](https://github.com/Heroic-Games-Launcher/wine-crossover) mirror, used for the Wine Crossover runtime.
 - [Ludusavi](https://github.com/mtkennerly/ludusavi-manifest), for finding where games keep their saves.
 - [innoextract](https://constexpr.org/innoextract/), for unpacking installers without Wine.

### Inspirations
 - [GameNative](https://github.com/utkarshdalal/GameNative): The Inspiration.
 - [Whisky](https://github.com/Whisky-App/Whisky), RIP.
 - [Heroic Games Launcher](https://heroicgameslauncher.com).
 - [Sikarugir](https://github.com/Sikarugir-App/Sikarugir).