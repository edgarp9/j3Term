# j3Term

j3Term is a cross-platform desktop terminal application built with Rust.

## License And Notices

j3Term is distributed under GPL-3.0-or-later. Source and binary release
packages include:

- `LICENSE`
- `NOTICE`
- `THIRD_PARTY_NOTICES.txt`
- `about.txt`
- `LICENSES/`

## Linux Desktop Integration

Linux distribution files:

- `j3term` executable
- `LICENSE`
- `NOTICE`
- `THIRD_PARTY_NOTICES.txt`
- `about.txt`
- `LICENSES/`
- `icon.svg`
- `icon.png` fallback, recommended

Install the user desktop entry and icon explicitly:

```bash
./j3term --install
./j3term
./j3term --uninstall
```

The Linux application id and icon name are `io.github.edgarp9.j3Term`.
