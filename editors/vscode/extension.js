// PHX for VS Code: starts `phx lsp` for diagnostics and hover types.
// Syntax highlighting needs no code; it comes from the grammars in syntaxes/.

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function activate(context) {
  const config = vscode.workspace.getConfiguration("phx");
  if (!config.get("lsp.enabled", true)) {
    return;
  }
  // `phx.path` is a machine-scoped setting, so a workspace cannot point it at
  // a program of its own.
  const command = config.get("path") || "phx";
  client = new LanguageClient(
    "phx",
    "PHX Language Server",
    { command, args: ["lsp"] },
    {
      documentSelector: [
        { scheme: "file", language: "phx" },
        { scheme: "file", language: "phx-template" },
      ],
    },
  );
  client.start().catch((err) => {
    vscode.window.showWarningMessage(
      `PHX: could not start "${command} lsp" (${err.message}). Install phx from https://phx-lang.org or set "phx.path".`,
    );
  });
  context.subscriptions.push({ dispose: () => client && client.stop() });
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
