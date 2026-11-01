// Tokenizes sample PHX sources with the extension's grammars (the same
// TextMate engine VS Code uses) and checks the scopes of key tokens.
// Run: npm test

const fs = require("fs");
const path = require("path");
const vsctm = require("vscode-textmate");
const oniguruma = require("vscode-oniguruma");

const root = path.join(__dirname, "..");
const wasm = fs.readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm")).buffer;

const grammars = {
  "source.phx": "syntaxes/phx.tmLanguage.json",
  "text.html.phx": "syntaxes/phx-html.tmLanguage.json",
};

const registry = new vsctm.Registry({
  onigLib: oniguruma.loadWASM(wasm).then(() => ({
    createOnigScanner: (patterns) => new oniguruma.OnigScanner(patterns),
    createOnigString: (s) => new oniguruma.OnigString(s),
  })),
  loadGrammar: async (scope) => {
    // HTML, SQL, JS and CSS come with VS Code; here they are left out, which
    // only means their content is not colored in the test.
    if (!grammars[scope]) return null;
    const file = path.join(root, grammars[scope]);
    return vsctm.parseRawGrammar(fs.readFileSync(file, "utf8"), file);
  },
});

/** All (text, scopes) tokens of `source` under `scope`. */
async function tokenize(scope, source) {
  const grammar = await registry.loadGrammar(scope);
  let stack = vsctm.INITIAL;
  const out = [];
  for (const line of source.split("\n")) {
    const r = grammar.tokenizeLine(line, stack);
    for (const t of r.tokens) {
      out.push({ text: line.slice(t.startIndex, t.endIndex), scopes: t.scopes });
    }
    stack = r.ruleStack;
  }
  return out;
}

let failures = 0;
function expect(tokens, text, scope) {
  const t = tokens.find((t) => t.text === text);
  const ok = t && t.scopes.some((s) => s.startsWith(scope));
  if (!ok) {
    failures++;
    console.error(`FAIL: "${text}" should have ${scope}; got ${t ? t.scopes.join(" ") : "no such token"}`);
  }
}

(async () => {
  const code = await tokenize(
    "source.phx",
    [
      "<?phx",
      "import std.http.request;",
      "/** Greets someone. */",
      "function greet(string $name, int $times = 1): string {",
      '    return "Hello, {$name}!\\n";',
      "}",
      '$rows = $db->query(sql"SELECT * FROM users WHERE name = {$name}");',
      "$n = $user?->name ?? null; // fallback",
      "?>",
      '<p class="x"><?= greet($n) ?></p>',
    ].join("\n"),
  );
  expect(code, "<?phx", "punctuation.section.embedded.begin.phx");
  expect(code, "import", "keyword.control.import.phx");
  expect(code, " Greets someone. ", "comment.block.documentation.phx");
  expect(code, "greet", "entity.name.function.phx");
  expect(code, "string", "support.type.phx");
  expect(code, "$name", "variable.other.phx");
  expect(code, "{", "punctuation.definition.template-expression.begin.phx");
  expect(code, "\\n", "constant.character.escape.phx");
  expect(code, "sql", "storage.type.string.sql.phx");
  expect(code, "?->", "punctuation.accessor.phx");
  expect(code, "??", "keyword.operator.null-coalescing.phx");
  expect(code, "null", "constant.language.null.phx");
  expect(code, "// fallback", "comment.line.double-slash.phx");
  expect(code, "?>", "punctuation.section.embedded.end.phx");
  expect(code, "<?=", "punctuation.section.embedded.begin.phx");

  const template = await tokenize(
    "text.html.phx",
    ['<a href="<?= safe_url($u) ?>">link</a>', "<?phx foreach ($items as $i): ?>", "<li><?= $i ?></li>", "<?phx endforeach; ?>"].join("\n"),
  );
  expect(template, "safe_url", "entity.name.function.phx");
  expect(template, "$u", "variable.other.phx");
  expect(template, "foreach", "keyword.control.phx");
  expect(template, "endforeach", "keyword.control.phx");
  expect(template, "$i", "variable.other.phx");

  if (failures > 0) {
    console.error(`${failures} grammar check(s) failed`);
    process.exit(1);
  }
  console.log("grammar checks passed");
})();
