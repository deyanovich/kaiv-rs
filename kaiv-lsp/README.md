# kaiv-lsp

Language server for the [kaiv](https://kaiv.io) file family:
live per-file diagnostics from the reference pipeline. Thin by
design — a synchronous stdio server (no async runtime) that
runs the pipeline stage each document's extension calls for and
publishes the first error as a whole-line diagnostic carrying
the spec's stable error name as its code.

| Extensions | Check |
|---|---|
| `.kaiv` `.raiv` | compile + denormalize |
| `.daiv` | format declaration + lex |
| `.saiv` | schema compile |
| `.csaiv` | compiled-schema parse |
| `.taiv` | type-library check |
| `.faiv` `.maiv` `.msaiv` | lex under the file kind's rules |
| `.qaiv` | highlighting only (no query checker yet) |

Configuration resolution mirrors the `kaiv` CLI: the nearest
`kaiv.kaiv` up from the document's directory; offline otherwise.

## Install

    cargo install kaiv-lsp

## Editors

- **VS Code** — the
  [editors](https://gitlab.com/kaiv-format/editors) extension
  spawns `kaiv-lsp` automatically when it is on `PATH` (or set
  `kaiv.lsp.path`).
- **Neovim (0.11+)** — the
  [kaiv-vim](https://gitlab.com/kaiv-format/kaiv-vim) plugin
  ships a client config snippet in `nvim/kaiv-lsp.lua`.
- Any other LSP client: run `kaiv-lsp` over stdio for the
  `kaiv` filetype.

Diagnostics are line-granular in this release — the pipeline
reports lines, not columns, so the whole line is underlined.

Part of the [kaiv-rs](https://gitlab.com/kaiv-format/kaiv-rs)
workspace; the syntax-highlighting guide at
[kaiv.io/guides/highlighting.html](https://kaiv.io/guides/highlighting.html)
covers the full editor story.
