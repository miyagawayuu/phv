// Finds the phv executable the extension starts, as an absolute path.
//
// A bare `phv` would be looked up by the operating system, and on Windows that
// lookup tries the working directory (the first workspace folder) before PATH:
// opening a .phv file in a cloned folder would run that folder's `phv.exe`, also
// in Restricted Mode. So the extension never starts a bare name. It searches only
// the absolute PATH entries that are outside every workspace folder, and passes
// the absolute path it found.
//
// This module does not use the `vscode` API, so that it can be tested with node.

const nodePath = require("path");
const fs = require("fs");

/** Whether `p` is `folder` or inside it (case-insensitively on Windows). */
function isInside(p, folder, win) {
  const path = win ? nodePath.win32 : nodePath.posix;
  let a = path.resolve(p);
  let b = path.resolve(folder);
  if (win) {
    a = a.toLowerCase();
    b = b.toLowerCase();
  }
  const rel = path.relative(b, a);
  return rel !== ".." && !rel.startsWith(`..${path.sep}`) && !path.isAbsolute(rel);
}

function defaultIsFile(p) {
  try {
    if (!fs.statSync(p).isFile()) return false;
    if (process.platform !== "win32") fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function defaultRealpath(p) {
  try {
    return fs.realpathSync.native(p);
  } catch {
    return p;
  }
}

/** The value of `name` in `env`, whose keys are case-insensitive on Windows (`Path`). */
function envGet(env, name, win) {
  if (!win) return env[name];
  const key = Object.keys(env).find((k) => k.toUpperCase() === name && env[k] !== undefined);
  return key === undefined ? undefined : env[key];
}

// A Windows path with a drive (`C:\...`): the drive letter is the only place a `:` may be.
const DRIVE = /^[a-zA-Z]:\\/;
// A network path (`\\host\share\...`). The host starts with a letter or a digit: `\\?\` and `\\.\` are device paths,
// which Windows passes to the file system without its usual name rules, so the checks below would not hold for them.
const UNC = /^\\\\[a-zA-Z0-9][^\\]*\\[^\\]+\\/;

/**
 * Why a full Windows path (already normalised) is not accepted as `phv.path`, or null when it is. Only the drive
 * letter may hold a `:`: `phv.cmd:s.exe` is an alternate data stream of `phv.cmd` (a shell script), and the
 * extension check would see `.exe`.
 */
function windowsPathProblem(p) {
  if (/^\\\\[?.]/.test(p)) return "a device path (\\\\?\\ or \\\\.\\) is not used";
  if (p.startsWith("\\\\") && !UNC.test(p)) return "a network path must be \\\\host\\share\\...\\phv.exe";
  if (!DRIVE.test(p) && !UNC.test(p)) return "it must be a full path such as C:\\phv\\phv.exe";
  if ((DRIVE.test(p) ? p.slice(2) : p).includes(":")) {
    return "a `:` after the drive letter (an alternate data stream) is not used";
  }
  return null;
}

/**
 * Resolves the `phv.path` setting to an absolute executable.
 *
 * - An absolute setting is used as it is (it must be a file; on Windows an `.exe` or `.com` named by a drive path or
 *   a `\\host\share` path, with no `:` after the drive letter and no `\\?\` or `\\.\` prefix).
 * - A relative path (`bin/phv`, `.\phv.exe`) is refused: it would name a program in the workspace.
 * - A bare name (`phv`, the default) is searched in PATH: only absolute entries outside every workspace folder (on
 *   Windows only drive paths: a network entry, or one that is a link to a network path, is skipped, since the
 *   workspace folders cannot be recognised under every name a share gives them); on Windows with the extensions
 *   `.exe` and `.com` (in PATHEXT's order), the ones that start without a shell.
 *
 * Returns `{ path }` or `{ error }`.
 *
 * @param {object} o
 * @param {string | undefined} o.setting the `phv.path` setting
 * @param {string[]} o.workspaceFolders the file system paths of the workspace folders
 * @param {Record<string, string | undefined>} [o.env] the environment (default: process.env)
 * @param {string} [o.platform] default: process.platform
 * @param {(p: string) => boolean} [o.isFile] whether `p` is an executable file
 * @param {(p: string) => string} [o.realpath] the path with links resolved
 */
function resolvePhv(o) {
  const win = (o.platform || process.platform) === "win32";
  const path = win ? nodePath.win32 : nodePath.posix;
  const env = o.env || process.env;
  const isFile = o.isFile || defaultIsFile;
  const realpath = o.realpath || defaultRealpath;
  const folders = o.workspaceFolders || [];
  const setting = (o.setting || "").trim() || "phv";
  // On Windows, the extensions of programs that start without a shell (.cmd and .bat need cmd.exe).
  const runnable = [".exe", ".com"];

  if (path.isAbsolute(setting)) {
    const full = path.normalize(setting);
    if (win) {
      // On Windows `\phv.exe` or `C:phv.exe` is "absolute" only in part: it depends on the current drive or folder;
      // a `:` elsewhere names an alternate data stream; `\\?\` and `\\.\` skip the name rules the checks rely on.
      const problem = windowsPathProblem(full);
      if (problem) {
        return { error: `"phv.path" is "${setting}": ${problem}` };
      }
      // As in PATH: only a program that starts without a shell (not .cmd, .bat or .ps1).
      if (!runnable.includes(path.extname(full).toLowerCase())) {
        return { error: `"phv.path" must name an .exe (or .com) file, such as C:\\phv\\phv.exe (it is "${setting}")` };
      }
    }
    if (!isFile(full)) {
      return { error: `"phv.path" is "${setting}", which is not an executable file` };
    }
    return { path: full };
  }
  const separators = win ? /[\\/:]/ : /\//;
  if (separators.test(setting) || setting === "." || setting === "..") {
    return {
      error: `"phv.path" must be a full path or a bare name to look up in PATH (it is "${setting}"); a relative path is not used, as it would name a program in the workspace`,
    };
  }

  // Whether a PATH entry or a program in it is not to be used: in a workspace folder (as written or with links
  // resolved), or on Windows a network path (as written or after resolving links). A workspace folder has other
  // names over the network (`\\127.0.0.1\C$\work\repo`, `\\<this machine>\<a share>\...`, a mapped drive), which a
  // comparison cannot know all of, so a network entry is skipped: phv on a share is named by a full `phv.path`.
  const skipped = (p) => {
    const real = realpath(p);
    if (win && (p.startsWith("\\\\") || real.startsWith("\\\\"))) return true;
    return folders.some((f) => isInside(p, f, win) || isInside(real, f, win) || isInside(real, realpath(f), win));
  };

  let names = [setting];
  if (win) {
    const ext = path.extname(setting).toLowerCase();
    if (ext !== "" && runnable.includes(ext)) {
      names = [setting];
    } else {
      // PATHEXT's order, keeping only the runnable extensions.
      const pathext = (envGet(env, "PATHEXT", win) || ".COM;.EXE")
        .split(";")
        .map((e) => e.trim().toLowerCase())
        .filter((e) => runnable.includes(e));
      for (const e of runnable) if (!pathext.includes(e)) pathext.push(e);
      names = pathext.map((e) => setting + e);
    }
  }

  const entries = (envGet(env, "PATH", win) || "").split(path.delimiter);
  for (let dir of entries) {
    dir = dir.trim();
    if (win && dir.length >= 2 && dir.startsWith('"') && dir.endsWith('"')) dir = dir.slice(1, -1);
    // An empty or relative entry (`.`, `bin`) names a folder relative to the working directory.
    if (dir === "" || !path.isAbsolute(dir)) continue;
    // On Windows only a drive path (`C:\...`): `\phv` and `C:phv` are relative in part, `\\...` is a network path.
    if (win && !DRIVE.test(path.normalize(dir))) continue;
    if (skipped(dir)) continue;
    for (const name of names) {
      const candidate = path.join(dir, name);
      if (isFile(candidate) && !skipped(candidate)) {
        return { path: candidate };
      }
    }
  }
  const notSearched = win
    ? "folders inside the workspace, relative PATH entries and network entries are not searched"
    : "folders inside the workspace and relative PATH entries are not searched";
  return {
    error: `could not find "${setting}" in PATH (${notSearched}). Install phv from https://phv-lang.org, or set "phv.path" to its full path`,
  };
}

/**
 * The environment for phv: the extension host's, with NoDefaultCurrentDirectoryInExePath set so that programs phv
 * starts on Windows are not looked up in their working directory either.
 */
function childEnv(env) {
  return { ...(env || process.env), NoDefaultCurrentDirectoryInExePath: "1" };
}

module.exports = { resolvePhv, childEnv, isInside };
