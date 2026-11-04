// PHV for VS Code: starts `phv lsp` for diagnostics, completion, navigation, rename and quick
// fixes, and `phv dap` for the "phv" debug type. Syntax highlighting needs no code; it comes from
// the grammars in syntaxes/.

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function activate(context) {
  const config = vscode.workspace.getConfiguration("phv");
  // `phv.path` is a machine-scoped setting, so a workspace cannot point it at
  // a program of its own.
  const command = config.get("path") || "phv";
  // The debugger: `phv dap` speaks the Debug Adapter Protocol on stdin/stdout.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("phv", {
      createDebugAdapterDescriptor() {
        return new vscode.DebugAdapterExecutable(command, ["dap"]);
      },
    }),
  );
  if (!config.get("lsp.enabled", true)) {
    return;
  }
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
