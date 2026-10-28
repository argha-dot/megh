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