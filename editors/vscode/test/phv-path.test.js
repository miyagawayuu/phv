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
      "C:\\work\\repo\\phv.cmd:s.exe", // an alternate data stream of the workspace's phv.cmd
      "\\\\server\\share\\phv.exe",
      "\\\\server\\share\\phv.cmd:s.exe",
      "\\\\127.0.0.1\\C$\\work\\repo\\phv.exe", // the workspace through the admin share
      "\\\\?\\C:\\tools\\phv.exe",
      "\\\\.\\C:\\tools\\phv.exe",
      "\\\\?\\UNC\\server\\share\\phv.exe",
      "D:\\netlink\\phv.exe",
      "C:\\netexe\\phv.exe",
    ].map((f) => f.toLowerCase()),
  );
  const isFile = (p) => files.has(p.toLowerCase());
  // D:\link is a directory link to the workspace; D:\netlink one to the workspace's admin-share spelling; C:\netexe\phv.exe
  // a file link to the workspace's phv.exe over the admin share.
  const realpath = (p) => {
    const l = p.toLowerCase();
    if (l.startsWith("d:\\link")) return "C:\\work\\repo" + p.slice("D:\\link".length);
    if (l.startsWith("d:\\netlink")) return "\\\\127.0.0.1\\C$\\work\\repo" + p.slice("D:\\netlink".length);
    if (l === "c:\\netexe\\phv.exe") return "\\\\127.0.0.1\\C$\\work\\repo\\phv.exe";
    return p;
  };
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

  // Spellings of a full path (18th assessment, E-4). A `:` after the drive letter names an alternate data stream:
  // `phv.cmd:s.exe` is a stream of a shell script that the extension check would take for an .exe.
  for (const ads of [
    "C:\\work\\repo\\phv.cmd:s.exe",
    "C:\\tools\\phv.exe:x",
    "C:\\cmdonly\\phv.cmd::$DATA",
    "C:\\tools\\phv.exe::$DATA",
    "\\\\server\\share\\phv.cmd:s.exe",
    "\\\\server\\share\\phv.exe:x",
  ]) {
    const r = win(ads, "");
    checkError(`an alternate data stream ${ads}`, r);
    check(`the error of ${ads} names the stream`, (r.error || "").includes("alternate data stream"), true);
  }
  // `\\?\` and `\\.\` are device paths: Windows applies none of its name rules to them.
  for (const dev of [
    "\\\\?\\C:\\tools\\phv.exe",
    "//?/C:/tools/phv.exe",
    "\\\\.\\C:\\tools\\phv.exe",
    "\\\\?\\UNC\\server\\share\\phv.exe",
    "\\\\?\\",
    "\\??\\C:\\tools\\phv.exe",
  ]) {
    checkError(`a device path ${dev}`, win(dev, ""));
  }
  // A plain network path is the user's choice, as a drive path is; it must still name an .exe or .com.
  check("a network path to an .exe", win("\\\\server\\share\\phv.exe", ""), { path: "\\\\server\\share\\phv.exe" });
  check("a network path with forward slashes", win("//server/share/phv.exe", ""), { path: "\\\\server\\share\\phv.exe" });
  checkError("a network path without a file", win("\\\\server\\share", ""));
  checkError("a network path without a share", win("\\\\server", ""));
  checkError("a network path with an empty host", win("\\\\\\share\\phv.exe", ""));
  // The spellings the 18th assessment found handled must stay so.
  checkError("a trailing dot", win("C:\\tools\\phv.exe.", ""));
  check("a trailing space is trimmed", win("C:\\tools\\phv.exe ", ""), { path: "C:\\tools\\phv.exe" });
  checkError("quotes around a full path", win('"C:\\tools\\phv.exe"', ""));
  check("forward slashes", win("C:/tools/phv.exe", ""), { path: "C:\\tools\\phv.exe" });
  check("a full path into the workspace is the user's own choice", win("C:\\work\\repo\\phv.exe", ""), {
    path: "C:\\work\\repo\\phv.exe",
  });
  // PATH: a network entry is never searched (the workspace has names over the network that a comparison cannot know),
  // nor an entry or a program that is a link to a network path.
  checkError("the workspace's admin-share spelling in PATH", win("phv", "\\\\127.0.0.1\\C$\\work\\repo"));
  checkError("a network share in PATH", win("phv", "\\\\server\\share"));
  checkError("a device path in PATH", win("phv", "\\\\?\\C:\\tools"));
  checkError("a link to the workspace's admin-share spelling in PATH", win("phv", "D:\\netlink"));
  checkError("a program in PATH that is a link to the workspace over the admin share", win("phv", "C:\\netexe"));
  check(
    "network entries are skipped, the next drive entry is used",
    win("phv", "\\\\127.0.0.1\\C$\\work\\repo;\\\\server\\share;D:\\netlink;C:\\netexe;C:\\tools"),
    { path: "C:\\tools\\phv.exe" },
  );
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
    if (process.platform === "win32") {
      // An alternate data stream named like an .exe on the workspace's phv.cmd (NTFS only).
      const ads = path.join(ws, "phv.cmd") + ":s.exe";
      let hasAds = false;
      try {
        fs.writeFileSync(ads, "MZ\n");
        hasAds = fs.statSync(ads).isFile();
      } catch {}
      if (hasAds) {
        const r = resolvePhv({ setting: ads, workspaceFolders: [ws], env: { Path: "" } });
        checkError("a real alternate data stream as the setting", r);
      }
      // The workspace spelled through the admin share of its drive, when this machine has one.
      const unc = "\\\\127.0.0.1\\" + ws[0] + "$" + ws.slice(2);
      let hasShare = false;
      try {
        hasShare = fs.statSync(unc).isDirectory();
      } catch {}
      if (hasShare) {
        checkError("the workspace's admin-share spelling in a real PATH", here(unc));
      }
      checkError(
        "a real device path as the setting",
        resolvePhv({ setting: "\\\\?\\" + path.join(tools, real), workspaceFolders: [ws], env: { Path: "" } }),
      );
      check(
        "the same file by its drive path",
        resolvePhv({ setting: path.join(tools, real), workspaceFolders: [ws], env: { Path: "" } }),
        { path: path.join(tools, real) },
      );
    }
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
