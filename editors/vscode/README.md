# PHV for Visual Studio Code

Language support for [PHV](https://phv-lang.org):

- Syntax highlighting for `.phv` files and `.phv.html` templates, including HTML around `<?phv ... ?>` /
  `<?= ... ?>`, string interpolation (`"Hello, {$name}"`) and SQL in `sql"..."` literals.
- From the PHV language server (`phv lsp`):
  - errors and warnings as you type, and the type of an expression on hover;
  - completion of variables, properties and methods after `->` and `::`, functions, classes and keywords;
  - go to definition (F12) for variables, functions, classes, methods, properties, imports and the templates
    passed to `render()`;
  - find all references (Shift+F12) and rename (F2) across the project; a method is renamed with the methods that
    override it;
  - format document (as `phv fmt`), parameter hints while typing a call, the outline of a file and symbol search
    (Ctrl+T);
  - quick fixes (Ctrl+.): close names for unknown names, removing unused imports, `$_name` for unused variables;
  - semantic colouring of names, and the inferred type before variables (inlay hints).
- Debugging with `phv dap`: add a launch configuration of type `phv` (`"program": "${file}"` runs the open
  script), then set breakpoints and step through the code (F10, F11, Shift+F11); the Variables view opens lists,
  maps and objects, and hovering a variable shows its value.
- Comment toggling, bracket matching and auto-closing pairs.

## Installing

Install **PHV Language** from the Visual Studio Marketplace: search for it in the Extensions view, or run

```bash
code --install-extension phv-lang.phv-language
```

Or download `phv-<version>.vsix` from the [releases](https://github.com/miyagawayuu/phv/releases) and install it:

```bash
code --install-extension phv-1.2.0.vsix
```

Or, in VS Code, run **Extensions: Install from VSIX...** from the Command Palette.

## Requirements

The language server needs the `phv` executable. Download it from the
[releases](https://github.com/miyagawayuu/phv/releases) or build it with `cargo build --release`, and put it on
your `PATH` (or set `phv.path`). Highlighting works without it.

The extension starts `phv` by its full path, never by the bare name (on Windows a bare name is looked for in the
workspace folder first, so a cloned folder could run its own `phv.exe`). It looks in the absolute folders of
`PATH` that are outside every workspace folder; on Windows it starts `phv.exe` or `phv.com`, not `.cmd` or `.bat`
files, and skips network entries (`\\server\share`, or a link to one: a workspace folder has names over the network
that cannot all be recognised). A relative `phv.path` or `PATH` entry is not used. A full `phv.path` on Windows must
be a drive path (`C:\phv\phv.exe`) or a share path (`\\server\share\phv.exe`) to an `.exe` or `.com`, with no `:`
after the drive letter (an alternate data stream such as `phv.cmd:s.exe`) and no `\\?\` or `\\.\` prefix. If
nothing is found, a message says so.

## Settings

| Setting | Default | |
|---|---|---|
| `phv.path` | `phv` | The `phv` executable: a full path (on Windows an `.exe` or `.com` by its drive or share path, without an alternate data stream or a `\\?\` prefix), or a name looked for in `PATH`. Only a user setting: a workspace's `.vscode/settings.json` cannot change which program runs. |
| `phv.lsp.enabled` | `true` | Start the language server. |

## Building the extension

```bash
npm install
npm test          # checks the grammars and how phv is found
npx vsce package  # writes phv-language-<version>.vsix
code --install-extension phv-language-1.2.0.vsix
```
