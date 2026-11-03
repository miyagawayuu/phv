// Tokenizes sample PHV sources with the extension's grammars (the same
// TextMate engine VS Code uses) and checks the scopes of key tokens.
// Run: npm test

const fs = require("fs");
const path = require("path");
const vsctm = require("vscode-textmate");
const oniguruma = require("vscode-oniguruma");

const root = path.join(__dirname, "..");
const wasm = fs.readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm")).buffer;

const grammars = {
  "source.phv": "syntaxes/phv.tmLanguage.json",
  "text.html.phv": "syntaxes/phv-html.tmLanguage.json",
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
    "source.phv",
    [
      "<?phv",
      "import std.http.request;",
      "/** Greets someone. */",
      "function greet(string $name, int $times = 1): string {",
      '    return "Hello, {$name}!\\n";',
      "}",
      '$rows = $db->query(sql"SELECT * FROM users WHERE name = {$name}");',
      "$n = $user?->name ?? null; // fallback",
      '#[Deprecated("use hello()")]',
      "function old(): void {} # hash comment",
      "?>",
      '<p class="x"><?= greet($n) ?></p>',
    ].join("\n"),
  );
  expect(code, "<?phv", "punctuation.section.embedded.begin.phv");
  expect(code, "import", "keyword.control.import.phv");
  expect(code, " Greets someone. ", "comment.block.documentation.phv");
  expect(code, "greet", "entity.name.function.phv");
  expect(code, "string", "support.type.phv");
  expect(code, "$name", "variable.other.phv");
  expect(code, "{", "punctuation.definition.template-expression.begin.phv");
  expect(code, "\\n", "constant.character.escape.phv");
  expect(code, "sql", "storage.type.string.sql.phv");
  expect(code, "?->", "punctuation.accessor.phv");
  expect(code, "??", "keyword.operator.null-coalescing.phv");
  expect(code, "null", "constant.language.null.phv");
  expect(code, "// fallback", "comment.line.double-slash.phv");
  expect(code, "Deprecated", "entity.name.type.attribute.phv");
  expect(code, "#[", "punctuation.definition.attribute.begin.phv");
  expect(code, "# hash comment", "comment.line.number-sign.phv");
  expect(code, "?>", "punctuation.section.embedded.end.phv");
  expect(code, "<?=", "punctuation.section.embedded.begin.phv");

  const template = await tokenize(
    "text.html.phv",
    ['<a href="<?= safe_url($u) ?>">link</a>', "<?phv foreach ($items as $i): ?>", "<li><?= $i ?></li>", "<?phv endforeach; ?>"].join("\n"),
  );
  expect(template, "safe_url", "entity.name.function.phv");
  expect(template, "$u", "variable.other.phv");
  expect(template, "foreach", "keyword.control.phv");
  expect(template, "endforeach", "keyword.control.phv");
  expect(template, "$i", "variable.other.phv");

  if (failures > 0) {
    console.error(`${failures} grammar check(s) failed`);
    process.exit(1);
  }
  console.log("grammar checks passed");
})();
