# PHV for Visual Studio Code

Language support for [PHV](https://phv-lang.org):

- Syntax highlighting for `.phv` files and `.phv.html` templates, including HTML around `<?phv ... ?>` /
  `<?= ... ?>`, string interpolation (`"Hello, {$name}"`) and SQL in `sql"..."` literals.
- From the PHV language server (`phv lsp`):
  - errors and warnings as you type, and the type of an expression on hover;
  - completion of variables, properties and methods after `->` and `::`, functions, classes and keywords;
  - go to definition (F12) for variables, functions, classes, methods, properties, imports and the templates
    passed to `render()`.
- Comment toggling, bracket matching and auto-closing pairs.

## Installing

Install **PHV Language** from the Visual Studio Marketplace: search for it in the Extensions view, or run

```bash
code --install-extension phv-lang.phv-language
```

Or download `phv-<version>.vsix` from the [releases](https://github.com/miyagawayuu/phv/releases) and install it:

```bash
code --install-extension phv-0.8.0.vsix
```

Or, in VS Code, run **Extensions: Install from VSIX...** from the Command Palette.

## Requirements

The language server needs the `phv` executable. Download it from the
[releases](https://github.com/miyagawayuu/phv/releases) or build it with `cargo build --release`, and put it on
your `PATH` (or set `phv.path`). Highlighting works without it.

## Settings

| Setting | Default | |
|---|---|---|
| `phv.path` | `phv` | The `phv` executable. Only a user setting: a workspace's `.vscode/settings.json` cannot change which program runs. |
| `phv.lsp.enabled` | `true` | Start the language server. |

## Building the extension

```bash
npm install
npm test          # checks the grammars
npx vsce package  # writes phv-language-<version>.vsix
code --install-extension phv-language-0.8.0.vsix
```
