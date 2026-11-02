# PHX for Visual Studio Code

Language support for [PHX](https://phx-lang.org):

- Syntax highlighting for `.phx` files and `.phx.html` templates, including HTML around `<?phx ... ?>` /
  `<?= ... ?>`, string interpolation (`"Hello, {$name}"`) and SQL in `sql"..."` literals.
- Errors and warnings as you type, and the type of an expression on hover, from the PHX language server
  (`phx lsp`).
- Comment toggling, bracket matching and auto-closing pairs.

## Installing

Install **PHX Language** from the Visual Studio Marketplace: search for it in the Extensions view, or run

```bash
code --install-extension phx-lang.phx-language
```

Or download `phx-<version>.vsix` from the [releases](https://github.com/miyagawayuu/phx/releases) and install it:

```bash
code --install-extension phx-0.2.7.vsix
```

Or, in VS Code, run **Extensions: Install from VSIX...** from the Command Palette.

## Requirements

The language server needs the `phx` executable. Download it from the
[releases](https://github.com/miyagawayuu/phx/releases) or build it with `cargo build --release`, and put it on
your `PATH` (or set `phx.path`). Highlighting works without it.

## Settings

| Setting | Default | |
|---|---|---|
| `phx.path` | `phx` | The `phx` executable. Only a user setting: a workspace's `.vscode/settings.json` cannot change which program runs. |
| `phx.lsp.enabled` | `true` | Start the language server. |

## Building the extension

```bash
npm install
npm test          # checks the grammars
npx vsce package  # writes phx-language-<version>.vsix
code --install-extension phx-language-0.2.7.vsix
```
