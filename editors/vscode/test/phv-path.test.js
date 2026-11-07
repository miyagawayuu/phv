// Checks how the extension finds the phv executable (phv-path.js): never a program in a workspace folder, never
// through a relative PATH entry, always an absolute path.
// Run: npm test

const fs = require("fs");
const os = require("os");
const path = require("path");
const { resolvePhv, childEnv } = require("../phv-path");

let failures = 0;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) {
    failures++;
    console.error(`FAIL: ${name}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  }
}
function checkError(name, r) {
  if (!r.error || r.path !== undefined) {
    failures++;
    console.error(`FAIL: ${name}: want an error, got ${JSON.stringify(r)}`);
  }
}

// Windows, with a made-up file system (works on any machine).
{
  const files = new Set(
    [
      "C:\\work\\repo\\phv.exe",
      "C:\\work\\repo\\phv.com",
      "C:\\work\\repo\\phv.cmd",
      "C:\\work\\repo\\phv.bat",
      "C:\\work\\repo\\phv",
      "C:\\work\\repo\\bin\\phv.exe",
      "C:\\work\\repo\\bin\\phv.com",
      "C:\\cmdonly\\phv.cmd",
      "C:\\cmdonly\\phv.bat",
      "C:\\cmdonly\\phv.ps1",
      "C:\\cmdonly\\phv",
      "C:\\tools\\phv.exe",
      "C:\\both\\phv.exe",
      "C:\\both\\phv.com",
      "D:\\link\\phv.exe",
      "D:\\phv\\phv.exe",
    ].map((f) => f.toLowerCase()),
  );
  const isFile = (p) => files.has(p.toLowerCase());
  // D:\link is a directory link to the workspace.
  const realpath = (p) => (p.toLowerCase().startsWith("d:\\link") ? "C:\\work\\repo" + p.slice("D:\\link".length) : p);
  const win = (setting, PATH, extra = {}) =>
    resolvePhv({
      setting,
      workspaceFolders: ["C:\\work\\repo"],
      env: { Path: PATH, PATHEXT: ".COM;.EXE;.BAT;.CMD", ...extra },
      platform: "win32",
      isFile,
      realpath,
    });

  check(
    "workspace, its subfolders, relative and empty entries are skipped",
    win("phv", ".;C:\\work\\repo;C:\\work\\repo\\bin;bin;;.\\bin;C:\\tools"),
    { path: "C:\\tools\\phv.exe" },
  );
  checkError("only the workspace in PATH", win("phv", ".;C:\\work\\repo;C:\\work\\repo\\bin"));
  checkError("an empty PATH (the working directory is never searched)", win("phv", ""));
  checkError("a workspace folder spelled in another case", win(undefined, "c:\\WORK\\Repo\\BIN;c:\\work\\repo\\"));
  checkError("a link to the workspace", win("phv", "D:\\link"));
  checkError("phv.cmd and phv.bat are not started (they need a shell)", win("phv", "C:\\cmdonly"));
  check("PATHEXT's order", win("phv", "C:\\both"), { path: "C:\\both\\phv.com" });
  check("an .exe name", win("phv.exe", "C:\\both"), { path: "C:\\both\\phv.exe" });
  check("a quoted PATH entry", win("phv", '"C:\\tools"'), { path: "C:\\tools\\phv.exe" });
  check("the default when the setting is empty", win("", "C:\\work\\repo;C:\\tools"), { path: "C:\\tools\\phv.exe" });
  check("an absolute setting", win("D:\\phv\\phv.exe", ""), { path: "D:\\phv\\phv.exe" });
  check("an absolute setting in another case", win("D:\\PHV\\PHV.EXE", ""), { path: "D:\\PHV\\PHV.EXE" });
  check("an absolute .com", win("C:\\both\\phv.com", ""), { path: "C:\\both\\phv.com" });
  checkError("an absolute setting that is not a file", win("D:\\phv\\nothing.exe", "C:\\tools"));
  // As in PATH, only an .exe or .com is started (a .cmd or .bat needs a shell; a .ps1 is not a program).
  for (const file of ["C:\\cmdonly\\phv.cmd", "C:\\cmdonly\\phv.bat", "C:\\cmdonly\\phv.ps1", "C:\\cmdonly\\phv"]) {
    const r = win(file, "C:\\tools");
    checkError(`an absolute setting ${file}`, r);
    check(`the error of ${file} says what to use`, (r.error || "").includes(".exe"), true);
  }
  for (const rel of ["bin\\phv.exe", ".\\phv.exe", "phv/phv.exe", "C:phv.exe", "\\phv.exe", ".", ".."]) {
    checkError(`a relative setting ${rel}`, win(rel, "C:\\tools"));
  }
}

// Unix, with a made-up file system.
{
  const files = new Set(["/home/u/repo/phv", "/home/u/repo/bin/phv", "/usr/local/bin/phv", "/opt/phv/bin/phv"]);
  const unix = (setting, PATH) =>
    resolvePhv({
      setting,
      workspaceFolders: ["/home/u/repo", "/srv/other"],
      env: { PATH },
      platform: "linux",
      isFile: (p) => files.has(p),
      realpath: (p) => p,
    });
  check("workspace and relative entries are skipped", unix("phv", ".:bin::/home/u/repo:/home/u/repo/bin:/usr/local/bin"), {
    path: "/usr/local/bin/phv",
  });
  checkError("only the workspace in PATH", unix("phv", ".:/home/u/repo/bin"));
  check("an absolute setting", unix("/opt/phv/bin/phv", ""), { path: "/opt/phv/bin/phv" });
  checkError("a relative setting", unix("./phv", "/usr/local/bin"));
  checkError("a relative setting with a folder", unix("bin/phv", "/usr/local/bin"));
}

// This machine's real file system: a workspace that holds every kind of phv program.
{
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "phv-path-test-"));
  try {
    const ws = path.join(tmp, "repo");
    const bin = path.join(ws, "bin");
    const tools = path.join(tmp, "tools");
    fs.mkdirSync(bin, { recursive: true });
    fs.mkdirSync(tools);
    for (const dir of [ws, bin]) {
      for (const name of ["phv", "phv.exe", "phv.com", "phv.cmd", "phv.bat"]) {
        fs.writeFileSync(path.join(dir, name), "#!/bin/sh\n");
        fs.chmodSync(path.join(dir, name), 0o755);
      }
    }
    const here = (PATH) =>
      resolvePhv({ setting: "phv", workspaceFolders: [ws], env: { ...process.env, PATH, Path: undefined } });
    const d = path.delimiter;
    checkError("a real workspace with phv programs", here(["", ".", "bin", ws, bin].join(d)));
    const real = process.platform === "win32" ? "phv.exe" : "phv";
    fs.writeFileSync(path.join(tools, real), "#!/bin/sh\n");
    fs.chmodSync(path.join(tools, real), 0o755);
    check("a real PATH entry outside the workspace", here([".", ws, bin, tools].join(d)), {
      path: path.join(tools, real),
    });
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

check("phv's environment", childEnv({ A: "1" }), { A: "1", NoDefaultCurrentDirectoryInExePath: "1" });

if (failures > 0) {
  console.error(`${failures} phv path check(s) failed`);
  process.exit(1);
}
console.log("phv path checks passed");
