// PHV for VS Code: starts `phv lsp` for diagnostics, completion, navigation, rename and quick
// fixes, and `phv dap` for the "phv" debug type. Syntax highlighting needs no code; it comes from
// the grammars in syntaxes/.

const path = require("path");
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");
const { resolvePhv, childEnv } = require("./phv-path");

/** @type {LanguageClient | undefined} */
let client;

/**
 * The phv executable as an absolute path, or `{ error }`. Never a bare name: on Windows that would be looked up in
 * the working directory (a workspace folder) first. `phv.path` is a machine-scoped setting, so a workspace cannot
 * point it at a program of its own either.
 */
function findPhv() {
  const folders = (vscode.workspace.workspaceFolders || [])
    .filter((f) => f.uri.scheme === "file")
    .map((f) => f.uri.fsPath);
  return resolvePhv({ setting: vscode.workspace.getConfiguration("phv").get("path"), workspaceFolders: folders });
}

function activate(context) {
  const config = vscode.workspace.getConfiguration("phv");
  // The debugger: `phv dap` speaks the Debug Adapter Protocol on stdin/stdout. The program is found again for each
  // session, as the workspace folders may have changed.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("phv", {
      createDebugAdapterDescriptor() {
        const found = findPhv();
        if (found.error) {
          throw new Error(`PHV: ${found.error}`);
        }
        return new vscode.DebugAdapterExecutable(found.path, ["dap"], { env: childEnv() });
      },
    }),
  );
  if (!config.get("lsp.enabled", true)) {
    return;
  }
  const found = findPhv();
  if (found.error) {
    vscode.window.showWarningMessage(`PHV: the language server is not started: ${found.error}.`);
    return;
  }
  const command = found.path;
  client = new LanguageClient(
    "phv",
    "PHV Language Server",
    // The working directory is phv's own folder, not the workspace (the server takes the project from the editor).
    { command, args: ["lsp"], options: { cwd: path.dirname(command), env: childEnv() } },
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
