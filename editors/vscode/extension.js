// PHV for VS Code: starts `phv lsp` for diagnostics and hover types.
// Syntax highlighting needs no code; it comes from the grammars in syntaxes/.

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function activate(context) {
  const config = vscode.workspace.getConfiguration("phv");
  if (!config.get("lsp.enabled", true)) {
    return;
  }
  // `phv.path` is a machine-scoped setting, so a workspace cannot point it at
  // a program of its own.
  const command = config.get("path") || "phv";
  client = new LanguageClient(
    "phv",
    "PHV Language Server",
    { command, args: ["lsp"] },
    {
      documentSelector: [
        { scheme: "file", language: "phv" },
        { scheme: "file", language: "phv-template" },
      ],
    },
  );
  client.start().catch((err) => {
    vscode.window.showWarningMessage(
      `PHV: could not start "${command} lsp" (${err.message}). Install phv from https://phv-lang.org or set "phv.path".`,
    );
  });
  context.subscriptions.push({ dispose: () => client && client.stop() });
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
