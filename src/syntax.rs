use karet_core::{AppliedEdit, FoldRegions, Highlights, LineCol, Range};
use karet_syntax::{
    Folder, Indenter, LayeredHighlighter, SemanticCommentConfig, fold_by_indent,
    mark_semantic_comments,
};
use karet_text::TextBuffer;
use karet_treesitter::{LanguageId, LayeredParser, LayeredTree};

pub struct SyntaxState {
    parser: LayeredParser,
    highlighter: LayeredHighlighter,
    folder: Folder,
    indenter: Option<Indenter>,
    tree: Option<LayeredTree>,
    lang: LanguageId,
    raw_highlights: Option<Highlights>,
    pub rendered_highlights: Option<Highlights>,
    pub folds: FoldRegions,
    pending_edits: Vec<AppliedEdit>,
    semantic_config: SemanticCommentConfig,
}

impl SyntaxState {
    #[must_use]
    pub fn new(lang: LanguageId, text: &str) -> Self {
        let mut parser = LayeredParser::new();
        let mut highlighter = LayeredHighlighter::new();
        let mut folder = Folder::new();
        let tree = parser.parse(lang, text).ok();
        let indenter = tree.as_ref().map(Indenter::for_tree);
        let raw_highlights = tree.as_ref().map(|t| highlighter.highlight(t, text));
        let mut folds = tree
            .as_ref()
            .map_or_else(FoldRegions::default, |t| folder.analyze(t.root(), text));
        if folds.regions().is_empty() {
            folds = fold_by_indent(text, 4);
        }
        let semantic_config = SemanticCommentConfig::default();
        let rendered_highlights = raw_highlights
            .as_ref()
            .map(|raw| mark_semantic_comments(text, raw, &semantic_config));

        Self {
            parser,
            highlighter,
            folder,
            indenter,
            tree,
            lang,
            raw_highlights,
            rendered_highlights,
            folds,
            pending_edits: Vec::new(),
            semantic_config,
        }
    }

    pub fn record_edits(&mut self, edits: &[AppliedEdit]) {
        self.pending_edits.extend_from_slice(edits);
    }

    pub fn update(&mut self, text: &str) {
        if self.pending_edits.is_empty() {
            return;
        }

        let mut changed = None;
        if let Some(mut tree) = self.tree.take() {
            match self
                .parser
                .reparse_changed(&mut tree, &self.pending_edits, text)
            {
                Ok(ranges) => {
                    changed = Some(ranges);
                    self.tree = Some(tree);
                }
                Err(_) => {
                    self.tree = self.parser.parse(self.lang, text).ok();
                }
            }
        } else {
            self.tree = self.parser.parse(self.lang, text).ok();
        }

        if let Some(tree) = &self.tree {
            let raw = if let Some(ranges) = changed
                && let Some(prev) = &self.raw_highlights
            {
                self.highlighter
                    .rehighlight(tree, text, prev, &self.pending_edits, &ranges)
            } else {
                self.highlighter.highlight(tree, text)
            };
            self.rendered_highlights =
                Some(mark_semantic_comments(text, &raw, &self.semantic_config));
            self.raw_highlights = Some(raw);
            self.indenter = Some(Indenter::for_tree(tree));
            let mut folds = self.folder.analyze(tree.root(), text);
            if folds.regions().is_empty() {
                folds = fold_by_indent(text, 4);
            }
            self.folds = folds;
        } else {
            self.raw_highlights = None;
            self.rendered_highlights = None;
            self.indenter = None;
            self.folds = fold_by_indent(text, 4);
        }

        self.pending_edits.clear();
    }

    #[must_use]
    pub const fn lang(&self) -> LanguageId {
        self.lang
    }

    #[must_use]
    pub const fn tree(&self) -> Option<&LayeredTree> {
        self.tree.as_ref()
    }

    #[must_use]
    pub fn compute_newline_indent_range(
        &self,
        buffer: &TextBuffer,
        range: Range,
    ) -> Option<String> {
        let indenter = self.indenter.as_ref()?;
        let tree = self.tree.as_ref()?;
        let before = buffer.line_col_to_byte(range.start).ok()?;
        let after = buffer.line_col_to_byte(range.end).ok()?;
        indenter.newline_indent(tree, &buffer.text(), (before, after), "    ")
    }

    #[must_use]
    pub fn compute_newline_indent(
        &self,
        buffer: &TextBuffer,
        caret: LineCol,
        selection: Option<Range>,
    ) -> Option<String> {
        let range = selection.unwrap_or(Range {
            start: caret,
            end: caret,
        });
        self.compute_newline_indent_range(buffer, range)
    }
}

#[cfg(test)]
mod tests {
    use karet_editor::editing;
    use karet_treesitter::languages;

    use super::*;

    #[test]
    fn test_layered_syntax_highlighting_and_incremental_reparse() {
        let markdown_with_rust =
            "# Markdown Heading\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
        let mut syntax = SyntaxState::new(languages::MARKDOWN.unwrap(), markdown_with_rust);

        assert!(syntax.tree.is_some());
        let tree = syntax.tree.as_ref().unwrap();
        assert!(!tree.children().is_empty());
        assert!(
            tree.children()
                .iter()
                .any(|c| c.language() == languages::RUST.unwrap())
        );
        assert!(syntax.rendered_highlights.is_some());

        // Incremental reparse parity with cold parse
        let mut buffer = TextBuffer::from_text(markdown_with_rust);
        let edit = editing::insert(LineCol::new(4, 15), None, buffer.version(), " world");
        let applied = buffer.apply_simple(&edit.change).unwrap();
        syntax.record_edits(&applied.edits);
        syntax.update(&buffer.text());

        let cold_syntax = SyntaxState::new(languages::MARKDOWN.unwrap(), &buffer.text());
        assert_eq!(
            syntax.raw_highlights.as_ref().unwrap().all().len(),
            cold_syntax.raw_highlights.as_ref().unwrap().all().len()
        );
    }

    #[test]
    fn test_semantic_codetag_highlighting() {
        let text = "// TODO: implement this feature\n";
        let syntax = SyntaxState::new(languages::RUST.unwrap(), text);
        assert!(syntax.rendered_highlights.is_some());
        let rendered = syntax.rendered_highlights.unwrap();
        assert_ne!(rendered.all(), []);
    }

    #[test]
    fn test_grammar_indenter_on_enter() {
        let text = "fn test() {\n";
        let buffer = TextBuffer::from_text(text);
        let syntax = SyntaxState::new(languages::RUST.unwrap(), text);

        let indent = syntax.compute_newline_indent(&buffer, LineCol::new(0, 11), None);
        assert_eq!(indent.as_deref(), Some("    "));
    }

    #[test]
    fn test_indentation_based_folding_fallback() {
        let text = "section:\n    item 1\n    item 2\nother:\n    item 3\n";
        let folds = fold_by_indent(text, 4);
        assert_eq!(folds.regions().len(), 2);
        assert_eq!(folds.regions()[0].start, 0);
        assert_eq!(folds.regions()[0].end, 2);
        assert_eq!(folds.regions()[1].start, 3);
        assert_eq!(folds.regions()[1].end, 4);

        let syntax = SyntaxState::new(languages::PYTHON.unwrap(), text);
        assert_ne!(syntax.folds.regions(), []);
    }
}
