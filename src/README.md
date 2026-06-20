# j3Term

j3Term is a cross-platform desktop terminal application built with Rust.

## Linux Desktop Integration

Linux distribution files:

- `j3term` executable
- `icon.svg`
- `icon.png` fallback, recommended

Install the user desktop entry and icon explicitly:

```bash
./j3term --install
./j3term
./j3term --uninstall
```

The Linux application id and icon name are `io.github.edgarp9.j3Term`.
