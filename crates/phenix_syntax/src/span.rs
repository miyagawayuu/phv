use std::path::PathBuf;

/// A byte range inside one source file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: u32,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: u32, start: u32, end: u32) -> Span {
        Span { file, start, end }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

pub struct SourceFile {
    /// Name shown in diagnostics, e.g. `public/index.phx` or `<std>/http.phx`.
    pub name: String,
    pub path: Option<PathBuf>,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(name: String, path: Option<PathBuf>, text: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { name, path, text, line_starts }
    }

    pub fn line_starts(&self) -> &[u32] {
        &self.line_starts
    }

    /// 1-based line and column (column counted in characters).
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = line_index(&self.line_starts, offset);
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.text.len());
        let col = self.text.get(start..end).map(|s| s.chars().count()).unwrap_or(0);
        (line + 1, col + 1)
    }

    /// The text of a 1-based line, without its line break.
    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1] as usize;
        let end = self
            .line_starts
            .get(line)
            .map(|e| *e as usize)
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// Index of the line containing `offset`, given the byte offsets where lines start.
pub fn line_index(line_starts: &[u32], offset: u32) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i - 1,
    }
}

#[derive(Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, name: String, path: Option<PathBuf>, text: String) -> u32 {
        self.files.push(SourceFile::new(name, path, text));
        self.files.len() as u32 - 1
    }

    pub fn get(&self, id: u32) -> &SourceFile {
        &self.files[id as usize]
    }
}
