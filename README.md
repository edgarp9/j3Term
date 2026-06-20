# j3Term

j3Term is a Rust desktop terminal with tabs, PTY-backed shell sessions, and a
configurable command button panel.

j3Term is a small desktop terminal application written in Rust. It combines a
native terminal view with a configurable command panel so repeated shell commands
can be launched quickly from buttons.

## Status

This project is experimental and still under active development. It was created
with AI assistance using an in-house toolchain, and the implementation should be
treated as work in progress.

Testing is currently not sufficient for production use. The repository contains
unit tests for several domain and infrastructure paths, but coverage is
incomplete, and manual UI testing is limited.

## Features

- Desktop terminal application written in Rust.
- PTY-backed shell sessions using `portable-pty`.
- Terminal parsing and screen state based on `alacritty_terminal`.
- Native Win32 UI.
- Tabbed terminal sessions.
- Terminal scrollback, mouse selection, copy, and paste support.
- Resizable split view with a terminal area on the left and command panel on the right.
- Editable command categories and command buttons.
- Command argument tokens such as `{path}`, `{name}`, `{selectfile}`, `{selectdir}`, and `{inputtext}`.
- Configurable monospace terminal font.

## Repository Layout

The Rust crate lives under `src/`.

```text
src/
  Cargo.toml
  build_release.py
  docs/
  src/
    app.rs
    domain/
    infra/
```

## Requirements

- Rust toolchain with Cargo.
- Windows.

## Build and Run

From the repository root:

```bash
cd src
cargo build
cargo run
```

Run the test suite:

```bash
cd src
cargo test
```

Build a release binary:

```bash
cd src
python build_release.py --no-open
```

The release helper builds with Cargo and reports the expected binary folder.

## Usage

Start the app normally:

```bash
j3term
```

Start the first shell in a specific working directory:

```powershell
j3term C:\path\to\project
```

Pass a startup command to the first terminal session:

```bash
j3term cargo check
```

## Configuration

j3Term stores command panel and font settings in a TOML file next to the
executable. The settings file uses the executable name with a `.toml` extension.

## License

j3Term is licensed under the GNU General Public License v3.0. See
[`LICENSE`](LICENSE) for details.

## Icon Notice and Thanks

This project uses an icon from [Google Fonts Icons](https://fonts.google.com/icons)
(Material Symbols). Material Symbols are available under the
[Apache License Version 2.0](https://www.apache.org/licenses/LICENSE-2.0), as
documented by the
[Google Fonts Material Symbols guide](https://developers.google.com/fonts/docs/material_symbols).

Thank you to Google Fonts and the Material Symbols contributors for making these
icons available.
