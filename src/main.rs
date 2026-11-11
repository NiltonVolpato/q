mod keymap;
mod status_bar;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent,
        KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use karet_core::{AppliedEdit, FoldRegions, Highlights, LineCol, Range};
use karet_editor::{
    Editor, EditorState, Fold, GutterLane, Motion, editing,
    editing::LineComment,
    editing::autopair::{self, AutoClose, AutoPairs},
    resolve_folds,
};
use karet_filetype::{CommentTokens, all_file_types, file_type_for_path};
use karet_syntax::{
    Folder, Indenter, LayeredHighlighter, SemanticCommentConfig, fold_by_indent,
    mark_semantic_comments,
};
use karet_text::{
    TextBuffer,
    words::{class_run_at, line_span},
};
use karet_theme::Theme;
use karet_treesitter::{
    LanguageId, LayeredParser, LayeredTree, injections_query, language_id_from_injection_name,
    language_id_from_path,
};
use keymap::{Action, EditorMode, map_key};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Position, Rect},
};
use status_bar::{StatusBar, StatusBarConfig};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputDestination {
    File(PathBuf),
    Stdout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliArgs {
    pub mode: EditorMode,
    pub destination: OutputDestination,
}

/// Parse CLI arguments.
///
/// # Errors
/// Returns an error if an unknown option is provided, if multiple files are provided,
/// or if no file is provided when not in prompt mode.
fn parse_args<I, T>(args: I) -> anyhow::Result<Option<CliArgs>>
where
    I: IntoIterator<Item = T>,
    T: Into<String>,
{
    let mut args_iter = args.into_iter().map(Into::into);
    let _program_name = args_iter.next();

    let mut mode = EditorMode::Normal;
    let mut file_arg: Option<String> = None;

    for arg in args_iter {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "qe - inline terminal editor\n\n\
                     Usage: qe [OPTIONS] [FILE]\n\n\
                     Arguments:\n  \
                       [FILE]  Path to file to edit or create (use - or omit for pipeline)\n\n\
                     Options:\n  \
                       -p, --prompt  Prompt mode (⏎ accepts, ⇧⏎ newline, ⎋ aborts)\n  \
                       -h, --help    Print help"
                );
                return Ok(None);
            }
            "-p" | "--prompt" => {
                mode = EditorMode::Prompt;
            }
            arg if arg.starts_with('-') && arg != "-" => {
                anyhow::bail!("unknown option '{arg}'. Use --help for usage information.");
            }
            _ => {
                if file_arg.is_some() {
                    anyhow::bail!(
                        "unexpected argument '{arg}'. Exactly one file argument is expected."
                    );
                }
                file_arg = Some(arg);
            }
        }
    }

    let destination = match file_arg.as_deref() {
        None | Some("-") => {
            if mode != EditorMode::Prompt {
                anyhow::bail!(
                    "no file specified. To use qe as an interactive pipeline prompt, pass -p/--prompt."
                );
            }
            OutputDestination::Stdout
        }
        Some(path_str) => OutputDestination::File(PathBuf::from(path_str)),
    };

    Ok(Some(CliArgs { mode, destination }))
}

struct SyntaxState {
    parser: LayeredParser,
    highlighter: LayeredHighlighter,
    folder: Folder,
    indenter: Option<Indenter>,
    tree: Option<LayeredTree>,
    lang: LanguageId,
    raw_highlights: Option<Highlights>,
    rendered_highlights: Option<Highlights>,
    folds: FoldRegions,
    pending_edits: Vec<AppliedEdit>,
    semantic_config: SemanticCommentConfig,
}

impl SyntaxState {
    fn new(lang: LanguageId, text: &str) -> Self {
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

    fn record_edits(&mut self, edits: &[AppliedEdit]) {
        self.pending_edits.extend_from_slice(edits);
    }

    fn update(&mut self, text: &str) {
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

    fn compute_newline_indent_range(&self, buffer: &TextBuffer, range: Range) -> Option<String> {
        let indenter = self.indenter.as_ref()?;
        let tree = self.tree.as_ref()?;
        let before = buffer.line_col_to_byte(range.start).ok()?;
        let after = buffer.line_col_to_byte(range.end).ok()?;
        indenter.newline_indent(tree, &buffer.text(), (before, after), "    ")
    }

    fn compute_newline_indent(
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

fn save_buffer(buffer: &mut TextBuffer, path: &std::path::Path) -> anyhow::Result<()> {
    let result = if path.exists() {
        buffer.save(path)
    } else {
        buffer.save_new(path)
    };
    result.map(|_| ()).map_err(|e| match e {
        karet_text::TextError::Conflict => {
            anyhow::anyhow!(
                "conflict: file was modified externally on disk: {}",
                path.display()
            )
        }
        other => anyhow::anyhow!("{other}"),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardStage {
    RawMode,
    MouseCapture,
    FullyInitialized,
    Restored,
}

struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<File>>>,
    viewport_area: Option<Rect>,
    stage: GuardStage,
}

impl TerminalGuard {
    fn init(lines: u16) -> anyhow::Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| anyhow::anyhow!("failed to open /dev/tty: {e}"))?;

        enable_raw_mode()?;
        let mut guard = Self {
            terminal: None,
            viewport_area: None,
            stage: GuardStage::RawMode,
        };
        let mut backend = CrosstermBackend::new(tty);
        execute!(
            backend,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )?;
        guard.stage = GuardStage::MouseCapture;
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(lines),
            },
        )?;
        guard.terminal = Some(terminal);
        guard.stage = GuardStage::FullyInitialized;
        Ok(guard)
    }

    const fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<File>> {
        self.terminal.as_mut().expect("terminal initialized")
    }

    const fn set_viewport_area(&mut self, area: Rect) {
        self.viewport_area = Some(area);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.stage == GuardStage::Restored {
            return;
        }
        if self.stage == GuardStage::FullyInitialized
            && let Some(terminal) = self.terminal.as_mut()
        {
            let _ = terminal.clear();
            if let Some(area) = self.viewport_area {
                let _ = terminal.set_cursor_position(Position::new(area.x, area.y));
            }
            let _ = terminal.show_cursor();
        }
        if matches!(
            self.stage,
            GuardStage::MouseCapture | GuardStage::FullyInitialized
        ) {
            if let Some(terminal) = self.terminal.as_mut() {
                let _ = execute!(
                    terminal.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    DisableMouseCapture
                );
            } else if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                let mut backend = CrosstermBackend::new(tty);
                let _ = execute!(backend, PopKeyboardEnhancementFlags, DisableMouseCapture);
            }
        }
        if self.stage != GuardStage::Restored {
            let _ = disable_raw_mode();
        }
        self.stage = GuardStage::Restored;
    }
}

fn split_viewport(area: Rect) -> (Rect, Rect) {
    let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
        .flex(Flex::End)
        .areas(area);
    [top, bottom].into()
}

fn tokens_for_language(lang: LanguageId) -> CommentTokens {
    all_file_types()
        .iter()
        .find(|ft| ft.grammar().and_then(language_id_from_injection_name) == Some(lang))
        .map_or(
            CommentTokens::NONE,
            karet_filetype::FileType::comment_tokens,
        )
}

fn probe_comment_pos(buffer: &TextBuffer, range: Range) -> LineCol {
    let from = range.start.col as usize;
    let col = buffer.line(range.start.line as usize).and_then(|text| {
        let indices: Vec<usize> = text
            .chars()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .collect();
        indices
            .iter()
            .copied()
            .find(|&i| i >= from)
            .or_else(|| indices.iter().copied().take_while(|&i| i < from).last())
            .and_then(|i| u32::try_from(i).ok())
    });
    LineCol::new(range.start.line, col.unwrap_or(range.start.col))
}

fn comment_tokens_at(
    destination: &OutputDestination,
    buffer: &TextBuffer,
    pos: LineCol,
    syntax_state: Option<&SyntaxState>,
) -> CommentTokens {
    let own = match destination {
        OutputDestination::File(path) => file_type_for_path(path).comment_tokens(),
        OutputDestination::Stdout => CommentTokens::NONE,
    };
    let Some(syntax) = syntax_state else {
        return own;
    };
    let Some(tree) = &syntax.tree else {
        return own;
    };
    if injections_query(syntax.lang).is_none() {
        return own;
    }
    let Ok(byte) = buffer.line_col_to_byte(pos) else {
        return own;
    };
    let layers = tree.layers_at(byte);
    let host = layers
        .iter()
        .rposition(|layer| {
            layer
                .named_ancestors_at(byte)
                .iter()
                .any(|node| node.kind.contains("comment"))
        })
        .unwrap_or(0);
    layers
        .iter()
        .skip(host)
        .map(|layer| layer.language())
        .map(|lang| {
            if lang == syntax.lang {
                own
            } else {
                tokens_for_language(lang)
            }
        })
        .find(|tokens| !tokens.is_empty())
        .unwrap_or(own)
}

fn apply_single_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    edit: &editing::Edit,
) -> anyhow::Result<bool> {
    let applied = buffer.apply_simple(&edit.change)?;
    if let Some(syn) = syntax_state {
        syn.record_edits(&applied.edits);
    }
    state.goto(buffer, edit.caret);
    Ok(true)
}

fn apply_opt_single_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    edit: Option<editing::Edit>,
) -> anyhow::Result<bool> {
    edit.map_or_else(
        || Ok(false),
        |e| apply_single_edit(buffer, state, syntax_state, &e),
    )
}

fn apply_multi_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    multi: editing::MultiEdit,
) -> anyhow::Result<bool> {
    let applied = buffer.apply_simple(&multi.change)?;
    if let Some(syn) = syntax_state {
        syn.record_edits(&applied.edits);
    }
    state.set_cursor_state(buffer, multi.cursors);
    Ok(true)
}

fn apply_opt_multi_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    multi: Option<editing::MultiEdit>,
) -> anyhow::Result<bool> {
    multi.map_or_else(
        || Ok(false),
        |m| apply_multi_edit(buffer, state, syntax_state, m),
    )
}

fn toggle_comment(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
) -> anyhow::Result<bool> {
    let pos = probe_comment_pos(buffer, state.cursors().primary().range());
    let tokens = comment_tokens_at(destination, buffer, pos, syntax_state.as_ref());
    let token = match (tokens.line(), tokens.block()) {
        (Some(prefix), _) => LineComment::Prefix(prefix),
        (None, Some((open, close))) => LineComment::Wrap { open, close },
        (None, None) => return Ok(false),
    };
    let multi = editing::toggle_line_comment(buffer, state.cursors(), buffer.version(), token);
    apply_opt_multi_edit(buffer, state, syntax_state, multi)
}

fn auto_pairs_for_destination(destination: &OutputDestination) -> &'static [(char, char)] {
    match destination {
        OutputDestination::File(path) => file_type_for_path(path).auto_pairs(),
        OutputDestination::Stdout => {
            file_type_for_path(std::path::Path::new("prompt.txt")).auto_pairs()
        }
    }
}

fn resolve_auto_pairs<'a>(
    destination: &OutputDestination,
    syntax_state: Option<&'a SyntaxState>,
) -> AutoPairs<'a> {
    AutoPairs {
        pairs: auto_pairs_for_destination(destination),
        brackets: AutoClose::LanguageDefined,
        quotes: AutoClose::LanguageDefined,
        highlights: syntax_state.and_then(|s| s.rendered_highlights.as_ref()),
    }
}

fn handle_insert_char(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
    c: char,
) -> anyhow::Result<bool> {
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    if let Some(typed) = autopair::type_char(
        buffer,
        state.cursors(),
        buffer.version(),
        c,
        &rules,
        auto_closers,
    ) {
        *auto_closers = typed.closers;
        if let Some(edit) = typed.edit {
            apply_multi_edit(buffer, state, syntax_state, edit)
        } else {
            state.set_cursor_state(buffer, typed.cursors);
            Ok(true)
        }
    } else {
        auto_closers.clear();
        let edit = editing::insert(
            state.cursor(),
            state.selection_range(),
            buffer.version(),
            &c.to_string(),
        );
        apply_single_edit(buffer, state, syntax_state, &edit)
    }
}

fn handle_insert_newline(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    auto_closers.clear();
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    let indent_fn = |range: Range| {
        syntax_state
            .as_ref()
            .and_then(|syn| syn.compute_newline_indent_range(buffer, range))
    };
    if let Some(edit) = autopair::newline_indented(
        buffer,
        state.cursors(),
        buffer.version(),
        &rules,
        "    ",
        &indent_fn,
    ) {
        apply_multi_edit(buffer, state, syntax_state, edit)
    } else {
        let indent = syntax_state.as_ref().and_then(|syn| {
            syn.compute_newline_indent(buffer, state.cursor(), state.selection_range())
        });
        let edit = editing::newline_indented(
            state.cursor(),
            state.selection_range(),
            buffer,
            buffer.version(),
            indent.as_deref(),
        );
        apply_single_edit(buffer, state, syntax_state, &edit)
    }
}

fn handle_backspace(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    auto_closers.clear();
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    if let Some(edit) = autopair::backspace(buffer, state.cursors(), buffer.version(), &rules) {
        apply_multi_edit(buffer, state, syntax_state, edit)
    } else {
        apply_opt_single_edit(
            buffer,
            state,
            syntax_state,
            editing::backspace(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ),
        )
    }
}

fn handle_motion_action(
    action: Action,
    buffer: &TextBuffer,
    state: &mut EditorState,
    auto_closers: &mut Vec<LineCol>,
) -> bool {
    let (motion, extend) = match action {
        Action::MoveLeft { extend } => (Motion::Left, extend),
        Action::MoveRight { extend } => (Motion::Right, extend),
        Action::MoveUp { extend } => (Motion::Up, extend),
        Action::MoveDown { extend } => (Motion::Down, extend),
        Action::MoveWordLeft { extend } => (Motion::WordLeft, extend),
        Action::MoveWordRight { extend } => (Motion::WordRight, extend),
        Action::MoveLineStart { extend } => (Motion::LineStart, extend),
        Action::MoveLineEnd { extend } => (Motion::LineEnd, extend),
        Action::MoveDocStart { extend } => (Motion::DocStart, extend),
        Action::MoveDocEnd { extend } => (Motion::DocEnd, extend),
        Action::PageUp { extend } => (Motion::PageUp, extend),
        Action::PageDown { extend } => (Motion::PageDown, extend),
        Action::SelectAll => {
            state.select_all(buffer);
            auto_closers.clear();
            return true;
        }
        _ => return false,
    };
    state.move_carets(buffer, motion, extend);
    auto_closers.clear();
    true
}

fn handle_edit_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    match action {
        Action::InsertChar(c) => {
            handle_insert_char(buffer, state, destination, syntax_state, auto_closers, c)
        }
        Action::Indent => {
            auto_closers.clear();
            apply_opt_multi_edit(
                buffer,
                state,
                syntax_state,
                editing::indent(buffer, state.cursors(), buffer.version(), "    ", 4),
            )
        }
        Action::Dedent => {
            auto_closers.clear();
            apply_opt_multi_edit(
                buffer,
                state,
                syntax_state,
                editing::dedent(buffer, state.cursors(), buffer.version(), "    ", 4),
            )
        }
        Action::InsertNewline => {
            handle_insert_newline(buffer, state, destination, syntax_state, auto_closers)
        }
        Action::DeleteBackward => {
            handle_backspace(buffer, state, destination, syntax_state, auto_closers)
        }
        Action::DeleteForward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_forward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
            )
        }
        Action::DeleteWordBackward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_word_backward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
            )
        }
        Action::DeleteWordForward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_word_forward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
            )
        }
        _ => Ok(false),
    }
}

fn handle_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    if handle_motion_action(action, buffer, state, auto_closers) {
        return Ok(false);
    }
    if handle_edit_action(
        action,
        buffer,
        state,
        destination,
        syntax_state,
        auto_closers,
    )? {
        return Ok(true);
    }

    match action {
        Action::Undo => {
            auto_closers.clear();
            if let Some(applied) = buffer.undo() {
                if let Some(syn) = syntax_state {
                    syn.record_edits(&applied.edits);
                }
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::Redo => {
            auto_closers.clear();
            if let Some(applied) = buffer.redo() {
                if let Some(syn) = syntax_state {
                    syn.record_edits(&applied.edits);
                }
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::ToggleComment => {
            auto_closers.clear();
            toggle_comment(buffer, state, destination, syntax_state)
        }
        _ => Ok(false),
    }
}

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragGranularity {
    Character,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
struct EditorDrag {
    anchor: (LineCol, LineCol),
    granularity: DragGranularity,
    pointer: (u16, u16),
}

#[derive(Clone, Copy, Debug, Default)]
struct ClickTracker {
    last_click: Option<(Instant, u16, u16)>,
    click_streak: u8,
}

impl ClickTracker {
    fn register_click(&mut self, col: u16, row: u16) -> u8 {
        let now = Instant::now();
        let continues = self.last_click.is_some_and(|(t, c, r)| {
            now.duration_since(t) < MULTI_CLICK_INTERVAL
                && c.abs_diff(col) <= 1
                && r.abs_diff(row) <= 1
        });
        let streak = if continues {
            self.click_streak.saturating_add(1).min(3)
        } else {
            1
        };
        self.last_click = Some((now, col, row));
        self.click_streak = streak;
        streak
    }

    const fn reset(&mut self) {
        self.last_click = None;
        self.click_streak = 0;
    }
}

fn drag_span(
    buffer: &TextBuffer,
    pos: LineCol,
    granularity: DragGranularity,
) -> (LineCol, LineCol) {
    match granularity {
        DragGranularity::Character => (pos, pos),
        DragGranularity::Word => class_run_at(buffer, pos),
        DragGranularity::Line => line_span(buffer, pos.line),
    }
}

fn apply_drag_selection(
    buffer: &TextBuffer,
    state: &mut EditorState,
    editor_area: Rect,
    resolved_folds: &[Fold],
    drag: EditorDrag,
    col: u16,
    row: u16,
) {
    if drag.granularity == DragGranularity::Character {
        let pos = state.pos_at(editor_area, buffer, resolved_folds, col, row);
        state.extend_to(buffer, pos);
        return;
    }

    let char_pos = state.char_pos_at(editor_area, buffer, resolved_folds, col, row);
    let (start, end) = drag_span(buffer, char_pos, drag.granularity);
    if char_pos < drag.anchor.0 {
        state.set_selection(buffer, drag.anchor.1, start);
    } else {
        state.set_selection(buffer, drag.anchor.0, end);
    }
}

struct MouseContext<'a> {
    editor_area: Rect,
    folds: &'a FoldRegions,
    resolved_folds: &'a [Fold],
    folded: &'a mut BTreeSet<u32>,
    auto_closers: &'a mut Vec<LineCol>,
    click_tracker: &'a mut ClickTracker,
    editor_drag: &'a mut Option<EditorDrag>,
}

fn handle_gutter_or_ellipsis_click(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    ctx: &mut MouseContext<'_>,
) -> bool {
    if let Some(line) = state.fold_ellipsis_at(mouse.column, mouse.row) {
        ctx.folded.remove(&line);
        ctx.click_tracker.reset();
        *ctx.editor_drag = None;
        return true;
    }

    let Some(hit) = state.gutter_hit_at(
        ctx.editor_area,
        buffer,
        ctx.resolved_folds,
        mouse.column,
        mouse.row,
    ) else {
        return false;
    };

    ctx.click_tracker.reset();
    match hit.lane {
        GutterLane::Fold => {
            *ctx.editor_drag = None;
            let Some(region) = ctx
                .folds
                .regions()
                .iter()
                .find(|r| r.start == hit.line)
                .copied()
            else {
                return true;
            };
            if !ctx.folded.remove(&hit.line) {
                ctx.folded.insert(hit.line);
                state.lift_carets_onto_header(
                    buffer,
                    Fold {
                        start: hit.line,
                        end: region.end,
                        collapsed: true,
                    },
                );
            }
        }
        GutterLane::LineNumber => {
            state.select_lines(buffer, hit.line, hit.line);
            let span = line_span(buffer, hit.line);
            *ctx.editor_drag = Some(EditorDrag {
                anchor: span,
                granularity: DragGranularity::Line,
                pointer: (mouse.column, mouse.row),
            });
        }
        _ => {
            *ctx.editor_drag = None;
        }
    }
    true
}

fn handle_mouse_down(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    ctx: &mut MouseContext<'_>,
) {
    if handle_gutter_or_ellipsis_click(mouse, buffer, state, ctx) {
        return;
    }

    let streak = ctx.click_tracker.register_click(mouse.column, mouse.row);
    let pos = state.pos_at(
        ctx.editor_area,
        buffer,
        ctx.resolved_folds,
        mouse.column,
        mouse.row,
    );
    let shift = mouse
        .modifiers
        .contains(crossterm::event::KeyModifiers::SHIFT);

    let granularity = if shift {
        DragGranularity::Character
    } else {
        match streak {
            1 => DragGranularity::Character,
            2 => DragGranularity::Word,
            _ => DragGranularity::Line,
        }
    };

    match streak {
        _ if shift => {
            state.extend_to(buffer, pos);
        }
        1 => {
            state.set_caret(buffer, pos);
        }
        2 => {
            let char_pos = state.char_pos_at(
                ctx.editor_area,
                buffer,
                ctx.resolved_folds,
                mouse.column,
                mouse.row,
            );
            let (anchor, head) = class_run_at(buffer, char_pos);
            state.set_selection(buffer, anchor, head);
        }
        _ => {
            let (anchor, head) = line_span(buffer, pos.line);
            state.set_selection(buffer, anchor, head);
        }
    }

    let anchor = drag_span(buffer, pos, granularity);
    *ctx.editor_drag = Some(EditorDrag {
        anchor,
        granularity,
        pointer: (mouse.column, mouse.row),
    });
}

fn handle_mouse_drag(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    ctx: &mut MouseContext<'_>,
) {
    let Some(drag) = *ctx.editor_drag else {
        return;
    };
    if let Some(live) = ctx.editor_drag.as_mut() {
        live.pointer = (mouse.column, mouse.row);
    }

    if mouse.row < ctx.editor_area.y {
        let diff = u32::from(ctx.editor_area.y - mouse.row);
        state.scroll_to_line(state.scroll_line().saturating_sub(diff));
    } else if mouse.row >= ctx.editor_area.bottom() {
        let diff = u32::from(mouse.row - ctx.editor_area.bottom() + 1);
        state.scroll_to_line(state.scroll_line().saturating_add(diff));
    }

    let clamped_row = mouse.row.clamp(
        ctx.editor_area.y,
        ctx.editor_area.bottom().saturating_sub(1),
    );
    let clamped_col = mouse.column.min(ctx.editor_area.right().saturating_sub(1));
    apply_drag_selection(
        buffer,
        state,
        ctx.editor_area,
        ctx.resolved_folds,
        drag,
        clamped_col,
        clamped_row,
    );
}

fn handle_mouse_event(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    ctx: &mut MouseContext<'_>,
) {
    ctx.auto_closers.clear();
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            handle_mouse_down(mouse, buffer, state, ctx);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            handle_mouse_drag(mouse, buffer, state, ctx);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            *ctx.editor_drag = None;
        }
        MouseEventKind::ScrollDown => {
            state.scroll_to_line(state.scroll_line().saturating_add(1));
        }
        MouseEventKind::ScrollUp => {
            state.scroll_to_line(state.scroll_line().saturating_sub(1));
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LoopControl {
    Continue { text_changed: bool },
    Exit(ExitCode),
    AcceptStdout,
}

fn handle_key_press(
    key: KeyEvent,
    in_confirm_discard: &mut bool,
    cli_args: &CliArgs,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<LoopControl> {
    if *in_confirm_discard {
        return Ok(match key.code {
            KeyCode::Char('y' | 'Y') => LoopControl::Exit(ExitCode::from(1)),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                *in_confirm_discard = false;
                LoopControl::Continue {
                    text_changed: false,
                }
            }
            _ => LoopControl::Continue {
                text_changed: false,
            },
        });
    }

    let action = map_key(key, cli_args.mode);
    match action {
        Action::Abort => {
            if buffer.is_dirty() {
                *in_confirm_discard = true;
                Ok(LoopControl::Continue {
                    text_changed: false,
                })
            } else {
                Ok(LoopControl::Exit(ExitCode::from(1)))
            }
        }
        Action::Quit => {
            if buffer.is_dirty() {
                *in_confirm_discard = true;
                Ok(LoopControl::Continue {
                    text_changed: false,
                })
            } else {
                Ok(LoopControl::Exit(ExitCode::SUCCESS))
            }
        }
        Action::Save => {
            if let OutputDestination::File(path) = &cli_args.destination {
                save_buffer(buffer, path)?;
            }
            Ok(LoopControl::Continue {
                text_changed: false,
            })
        }
        Action::SaveAndAccept => match &cli_args.destination {
            OutputDestination::File(path) => {
                save_buffer(buffer, path)?;
                Ok(LoopControl::Exit(ExitCode::SUCCESS))
            }
            OutputDestination::Stdout => Ok(LoopControl::AcceptStdout),
        },
        other => {
            let changed = handle_action(
                other,
                buffer,
                state,
                &cli_args.destination,
                syntax_state,
                auto_closers,
            )?;
            Ok(LoopControl::Continue {
                text_changed: changed,
            })
        }
    }
}

struct RenderContext<'a> {
    buffer: &'a TextBuffer,
    theme: &'a Theme,
    resolved_folds: &'a [Fold],
    highlights: Option<&'a Highlights>,
    lang_name: &'a str,
    file_name: &'a str,
    mode: EditorMode,
    in_confirm_discard: bool,
}

fn render_editor(
    frame: &mut ratatui::Frame,
    ctx: &RenderContext,
    state: &mut EditorState,
) -> (Rect, Rect) {
    let area = frame.area();
    let (top, bottom) = split_viewport(area);

    let mut editor_widget = Editor::new(ctx.buffer)
        .theme(ctx.theme)
        .folds(ctx.resolved_folds)
        .focused(true);
    if let Some(h) = ctx.highlights {
        editor_widget = editor_widget.highlights(h);
    }

    frame.render_stateful_widget(editor_widget, top, state);

    let total_lines = ctx.buffer.line_count();
    let bottom_line = (state.scroll_line() as usize).saturating_add(top.height as usize);
    let lines_below = total_lines.saturating_sub(bottom_line);
    let pct_below = (lines_below * 100)
        .checked_div(total_lines)
        .and_then(|pct| u8::try_from(pct).ok())
        .unwrap_or(0);

    let status_config = StatusBarConfig {
        cursor: state.cursor(),
        selection: state.selection_range(),
        language: ctx.lang_name,
        file_name: ctx.file_name,
        is_dirty: ctx.buffer.is_dirty(),
        mode: ctx.mode,
        in_confirm_discard: ctx.in_confirm_discard,
        pct_below,
    };
    StatusBar::new(ctx.theme, &status_config).draw(frame, bottom);

    if let Some((x, y)) = state.primary_caret_cell(top, ctx.buffer, ctx.resolved_folds) {
        frame.set_cursor_position(Position::new(x, y));
    }

    (area, top)
}

fn init_buffer(destination: &OutputDestination) -> anyhow::Result<(TextBuffer, String)> {
    match destination {
        OutputDestination::File(path) => {
            if path.is_dir() {
                anyhow::bail!("{}: Is a directory", path.display());
            }
            let buf = if path.exists() {
                TextBuffer::load(path).map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                TextBuffer::from_text("")
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("untitled")
                .to_owned();
            Ok((buf, name))
        }
        OutputDestination::Stdout => {
            if std::io::stdin().is_terminal() {
                Ok((TextBuffer::from_text(""), "[prompt]".to_owned()))
            } else {
                use std::io::Read as _;

                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input)?;
                let buf = TextBuffer::from_bytes(&input)
                    .map_err(|e| anyhow::anyhow!("stdin error: {e}"))?;
                Ok((buf, "[stdin]".to_owned()))
            }
        }
    }
}

enum EditorOutcome {
    AcceptStdout(TextBuffer),
    Exit(ExitCode),
}

fn handle_drag_autoscroll(
    drag: EditorDrag,
    state: &mut EditorState,
    buffer: &TextBuffer,
    editor_area: Rect,
    resolved_folds: &[Fold],
) {
    if drag.pointer.1 < editor_area.y {
        state.scroll_to_line(state.scroll_line().saturating_sub(1));
        let clamped_col = drag.pointer.0.min(editor_area.right().saturating_sub(1));
        apply_drag_selection(
            buffer,
            state,
            editor_area,
            resolved_folds,
            drag,
            clamped_col,
            editor_area.y,
        );
    } else if drag.pointer.1 >= editor_area.bottom() {
        state.scroll_to_line(state.scroll_line().saturating_add(1));
        let clamped_col = drag.pointer.0.min(editor_area.right().saturating_sub(1));
        let clamped_row = editor_area.bottom().saturating_sub(1);
        apply_drag_selection(
            buffer,
            state,
            editor_area,
            resolved_folds,
            drag,
            clamped_col,
            clamped_row,
        );
    }
}

fn run_editor_loop(guard: &mut TerminalGuard, cli_args: &CliArgs) -> anyhow::Result<EditorOutcome> {
    let (mut buffer, file_name) = init_buffer(&cli_args.destination)?;

    let (lang_id, lang_name) = match &cli_args.destination {
        OutputDestination::File(path) => {
            (language_id_from_path(path), file_type_for_path(path).name())
        }
        OutputDestination::Stdout => (None, "Plain Text"),
    };
    let mut syntax_state = lang_id.map(|lang| SyntaxState::new(lang, &buffer.text()));
    let mut plain_folds = if syntax_state.is_none() {
        fold_by_indent(&buffer.text(), 4)
    } else {
        FoldRegions::default()
    };

    let mut state = EditorState::new();
    let theme = Theme::dark();

    let mut folded: BTreeSet<u32> = BTreeSet::new();
    let mut viewport_area = Rect::default();
    let mut editor_area = Rect::default();
    let mut in_confirm_discard = false;
    let mut auto_closers: Vec<LineCol> = Vec::new();
    let mut click_tracker = ClickTracker::default();
    let mut editor_drag: Option<EditorDrag> = None;

    loop {
        let folds = syntax_state.as_ref().map_or(&plain_folds, |syn| &syn.folds);
        let resolved_folds = resolve_folds(folds, &folded);

        let render_ctx = RenderContext {
            buffer: &buffer,
            theme: &theme,
            resolved_folds: &resolved_folds,
            highlights: syntax_state
                .as_ref()
                .and_then(|syn| syn.rendered_highlights.as_ref()),
            lang_name,
            file_name: &file_name,
            mode: cli_args.mode,
            in_confirm_discard,
        };

        guard.terminal_mut().draw(|frame| {
            let (view_area, ed_area) = render_editor(frame, &render_ctx, &mut state);
            viewport_area = view_area;
            editor_area = ed_area;
        })?;
        guard.set_viewport_area(viewport_area);

        if event::poll(std::time::Duration::from_millis(50))? {
            let mut text_changed = false;

            match event::read()? {
                Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                    click_tracker.reset();
                    editor_drag = None;
                    match handle_key_press(
                        key,
                        &mut in_confirm_discard,
                        cli_args,
                        &mut buffer,
                        &mut state,
                        &mut syntax_state,
                        &mut auto_closers,
                    )? {
                        LoopControl::Continue {
                            text_changed: changed,
                        } => {
                            text_changed = changed;
                        }
                        LoopControl::Exit(code) => return Ok(EditorOutcome::Exit(code)),
                        LoopControl::AcceptStdout => {
                            return Ok(EditorOutcome::AcceptStdout(buffer));
                        }
                    }
                }
                Event::Mouse(mouse) if !in_confirm_discard => {
                    let mut ctx = MouseContext {
                        editor_area,
                        folds,
                        resolved_folds: &resolved_folds,
                        folded: &mut folded,
                        auto_closers: &mut auto_closers,
                        click_tracker: &mut click_tracker,
                        editor_drag: &mut editor_drag,
                    };
                    handle_mouse_event(mouse, &buffer, &mut state, &mut ctx);
                }
                _ => {}
            }

            if text_changed {
                if let Some(ref mut syn) = syntax_state {
                    syn.update(&buffer.text());
                } else {
                    plain_folds = fold_by_indent(&buffer.text(), 4);
                }
            }
        } else if let Some(drag) = editor_drag {
            handle_drag_autoscroll(drag, &mut state, &buffer, editor_area, &resolved_folds);
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args = std::env::args();
    let Some(cli_args) = parse_args(args)? else {
        return Ok(ExitCode::SUCCESS);
    };

    let mut guard = TerminalGuard::init(10)?;
    let outcome = run_editor_loop(&mut guard, &cli_args)?;
    drop(guard);

    match outcome {
        EditorOutcome::AcceptStdout(buffer) => {
            let mut stdout = std::io::stdout().lock();
            for chunk in buffer.chunks() {
                stdout.write_all(chunk.as_bytes())?;
            }
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        EditorOutcome::Exit(code) => Ok(code),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use karet_treesitter::languages;

    use super::*;

    #[test]
    fn test_split_viewport_allocates_editor_and_status_bar() {
        let area = Rect::new(0, 5, 80, 10);
        let (editor_area, status_bar_area) = split_viewport(area);

        assert_eq!(editor_area, Rect::new(0, 5, 80, 9));
        assert_eq!(status_bar_area, Rect::new(0, 14, 80, 1));
    }

    #[test]
    fn test_split_viewport_handles_minimal_height() {
        let area = Rect::new(0, 0, 80, 1);
        let (editor_area, status_bar_area) = split_viewport(area);

        assert_eq!(editor_area.height, 0);
        assert_eq!(status_bar_area.height, 1);
    }

    #[test]
    fn test_cursor_move_without_extend_clears_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();

        let changed = handle_action(
            Action::MoveRight { extend: false },
            &mut buffer,
            &mut state,
            &OutputDestination::Stdout,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(!changed);
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range();
        assert!(sel.is_none() || sel.unwrap().start == sel.unwrap().end);
    }

    #[test]
    fn test_cursor_move_with_extend_creates_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();

        handle_action(
            Action::MoveRight { extend: true },
            &mut buffer,
            &mut state,
            &OutputDestination::Stdout,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range().unwrap();
        assert_eq!(sel.start, LineCol::new(0, 0));
        assert_eq!(sel.end, LineCol::new(0, 1));
    }

    #[test]
    fn test_toggle_comment() {
        let mut buffer = TextBuffer::from_text("fn test() {}");
        let mut state = EditorState::new();
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "// fn test() {}");

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn test() {}");
    }

    #[test]
    fn test_multi_language_comment_toggling() {
        // Python (#)
        let mut py_buffer = TextBuffer::from_text("def hello(): pass");
        let mut py_state = EditorState::new();
        let py_dest = OutputDestination::File(PathBuf::from("test.py"));
        let mut py_syntax = Some(SyntaxState::new(
            languages::PYTHON.unwrap(),
            &py_buffer.text(),
        ));

        handle_action(
            Action::ToggleComment,
            &mut py_buffer,
            &mut py_state,
            &py_dest,
            &mut py_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(py_buffer.text(), "# def hello(): pass");

        handle_action(
            Action::ToggleComment,
            &mut py_buffer,
            &mut py_state,
            &py_dest,
            &mut py_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(py_buffer.text(), "def hello(): pass");

        // HTML (<!-- -->)
        let mut html_buffer = TextBuffer::from_text("<div>hello</div>");
        let mut html_state = EditorState::new();
        let html_dest = OutputDestination::File(PathBuf::from("index.html"));
        let mut html_syntax = Some(SyntaxState::new(
            languages::HTML.unwrap(),
            &html_buffer.text(),
        ));

        handle_action(
            Action::ToggleComment,
            &mut html_buffer,
            &mut html_state,
            &html_dest,
            &mut html_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(html_buffer.text(), "<!-- <div>hello</div> -->");

        handle_action(
            Action::ToggleComment,
            &mut html_buffer,
            &mut html_state,
            &html_dest,
            &mut html_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(html_buffer.text(), "<div>hello</div>");
    }

    #[test]
    fn test_indent_and_dedent() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();
        state.select_all(&buffer);
        let dest = OutputDestination::Stdout;

        handle_action(
            Action::Indent,
            &mut buffer,
            &mut state,
            &dest,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "    line 1\n    line 2");

        handle_action(
            Action::Dedent,
            &mut buffer,
            &mut state,
            &dest,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "line 1\nline 2");
    }

    #[test]
    fn test_layered_syntax_highlighting_and_incremental_reparse() {
        let text = "# Title\n\n```rust\nfn main() {}\n```\n";
        let syntax = SyntaxState::new(languages::MARKDOWN.unwrap(), text);
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
        let initial = "fn foo() -> u32 {\n    42\n}\n";
        let mut syntax = SyntaxState::new(languages::RUST.unwrap(), initial);
        let mut buffer = TextBuffer::from_text(initial);
        let mut state = EditorState::new();
        state.goto(&buffer, LineCol::new(1, 6));
        let edit = editing::insert(state.cursor(), None, buffer.version(), " + 10");
        let applied = buffer.apply_simple(&edit.change).unwrap();
        syntax.record_edits(&applied.edits);
        syntax.update(&buffer.text());

        let cold_syntax = SyntaxState::new(languages::RUST.unwrap(), &buffer.text());
        assert_eq!(
            syntax.raw_highlights.as_ref().unwrap().all(),
            cold_syntax.raw_highlights.as_ref().unwrap().all()
        );
    }

    #[test]
    fn test_grammar_indenter_on_enter() {
        let mut buffer = TextBuffer::from_text("fn main() {");
        let mut state = EditorState::new();
        state.goto(&buffer, LineCol::new(0, 11));
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));

        handle_action(
            Action::InsertNewline,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn main() {\n    ");
        assert_eq!(state.cursor(), LineCol::new(1, 4));
    }

    #[test]
    fn test_closing_bracket_auto_dedent() {
        let mut buffer = TextBuffer::from_text("fn main() {\n    ");
        let mut state = EditorState::new();
        state.goto(&buffer, LineCol::new(1, 4));
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('}'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn main() {\n}");
        assert_eq!(state.cursor(), LineCol::new(1, 1));
    }

    #[test]
    fn test_closing_bracket_dedent_without_preexisting_pair() {
        let mut buffer = TextBuffer::from_text("fn test() {\n    ");
        let mut state = EditorState::new();
        state.goto(&buffer, LineCol::new(1, 4));
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('}'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn test() {\n}");
        assert_eq!(state.cursor(), LineCol::new(1, 1));
    }

    #[test]
    fn test_autopair_insert_and_step_over() {
        let mut buffer = TextBuffer::from_text("");
        let mut state = EditorState::new();
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));
        let mut closers = Vec::new();

        // Typing '{' inserts '{}' with cursor inside
        handle_action(
            Action::InsertChar('{'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        assert_eq!(closers, vec![LineCol::new(0, 1)]);

        // Typing '}' steps over the closer
        handle_action(
            Action::InsertChar('}'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 2));
        assert_eq!(closers, []);
    }

    #[test]
    fn test_autopair_empty_backspace() {
        let mut buffer = TextBuffer::from_text("");
        let mut state = EditorState::new();
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('{'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 1));

        // Backspace inside '{}' deletes both brackets
        handle_action(
            Action::DeleteBackward,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "");
        assert_eq!(state.cursor(), LineCol::new(0, 0));
    }

    #[test]
    fn test_autopair_newline_bracket_expansion() {
        let mut buffer = TextBuffer::from_text("");
        let mut state = EditorState::new();
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('{'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 1));

        // Enter inside '{}' expands to block
        handle_action(
            Action::InsertNewline,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();
        assert_eq!(buffer.text(), "{\n    \n}");
        assert_eq!(state.cursor(), LineCol::new(1, 4));
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
    fn test_parse_args_normal_mode() {
        let args = vec!["qe", "src/main.rs"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Normal);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn test_parse_args_prompt_mode_with_file() {
        let args = vec!["qe", "-p", "commit.txt"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("commit.txt"))
        );
    }

    #[test]
    fn test_parse_args_prompt_mode_stdout_no_file() {
        let args = vec!["qe", "-p"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(parsed.destination, OutputDestination::Stdout);

        let args_long = vec!["qe", "--prompt"];
        let parsed_long = parse_args(args_long).unwrap().unwrap();
        assert_eq!(parsed_long.mode, EditorMode::Prompt);
        assert_eq!(parsed_long.destination, OutputDestination::Stdout);
    }

    #[test]
    fn test_parse_args_prompt_mode_stdout_explicit_dash() {
        let args = vec!["qe", "-p", "-"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(parsed.destination, OutputDestination::Stdout);
    }

    #[test]
    fn test_parse_args_help() {
        let args = vec!["qe", "--help"];
        let parsed = parse_args(args).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_args_missing_file_without_prompt_fails() {
        let args = vec!["qe"];
        assert!(parse_args(args).is_err());

        let args_dash = vec!["qe", "-"];
        assert!(parse_args(args_dash).is_err());
    }

    #[test]
    fn test_parse_args_unknown_flag_fails() {
        let args = vec!["qe", "--unknown", "file.txt"];
        assert!(parse_args(args).is_err());
    }

    #[test]
    fn test_parse_args_multiple_files_fails() {
        let args = vec!["qe", "file1.txt", "file2.txt"];
        assert!(parse_args(args).is_err());
    }

    #[test]
    fn test_language_detection_for_multiple_languages() {
        assert_eq!(file_type_for_path(Path::new("test.rs")).name(), "Rust");
        assert!(language_id_from_path(Path::new("test.rs")).is_some());

        assert_eq!(file_type_for_path(Path::new("script.py")).name(), "Python");
        assert!(language_id_from_path(Path::new("script.py")).is_some());

        assert_eq!(file_type_for_path(Path::new("app.js")).name(), "JavaScript");
        assert!(language_id_from_path(Path::new("app.js")).is_some());

        assert_eq!(file_type_for_path(Path::new("main.go")).name(), "Go");
        assert!(language_id_from_path(Path::new("main.go")).is_some());

        assert_eq!(
            file_type_for_path(Path::new("README.md")).name(),
            "Markdown"
        );
        assert!(language_id_from_path(Path::new("README.md")).is_some());
    }

    #[test]
    fn test_new_file_save_and_dirty_lifecycle() {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "qe_test_{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let mut buffer = TextBuffer::from_text("");
        assert!(!buffer.is_dirty());

        let edit = editing::insert(LineCol::new(0, 0), None, buffer.version(), "hello world");
        buffer.apply_simple(&edit.change).unwrap();
        assert!(buffer.is_dirty());

        buffer.save(&temp_file).unwrap();
        assert!(!buffer.is_dirty());

        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert_eq!(content.trim(), "hello world");

        let _ = std::fs::remove_file(temp_file);
    }

    #[test]
    fn test_handle_key_press_abort_and_confirm_discard_flow() {
        let cli_args = CliArgs {
            mode: EditorMode::Normal,
            destination: OutputDestination::File(PathBuf::from("test.txt")),
        };
        let mut buffer = TextBuffer::from_text("clean text");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;

        // Clean Esc: exits immediately with code 1
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Exit(code) if code == ExitCode::from(1)));
        assert!(!in_confirm_discard);

        // Edit buffer -> dirty
        let edit = editing::insert(LineCol::new(0, 0), None, buffer.version(), "dirty ");
        buffer.apply_simple(&edit.change).unwrap();
        assert!(buffer.is_dirty());

        // Dirty Esc: enters confirmation mode
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            res,
            LoopControl::Continue {
                text_changed: false
            }
        ));
        assert!(in_confirm_discard);

        // While in confirm discard, pressing 'n' cancels confirmation
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            res,
            LoopControl::Continue {
                text_changed: false
            }
        ));
        assert!(!in_confirm_discard);

        // Dirty Esc again -> confirm discard
        handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(in_confirm_discard);

        // Pressing 'y' confirms discard and exits with code 1
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('y'), crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Exit(code) if code == ExitCode::from(1)));
    }

    #[test]
    fn test_handle_key_press_prompt_mode_accept_file() {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "qe_prompt_test_{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cli_args = CliArgs {
            mode: EditorMode::Prompt,
            destination: OutputDestination::File(temp_file.clone()),
        };
        let mut buffer = TextBuffer::from_text("commit message");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;

        // In prompt mode, Enter saves and accepts with code 0
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Exit(code) if code == ExitCode::SUCCESS));
        assert!(temp_file.exists());
        let _ = std::fs::remove_file(temp_file);
    }

    #[test]
    fn test_handle_key_press_prompt_mode_accept_stdout() {
        let cli_args = CliArgs {
            mode: EditorMode::Prompt,
            destination: OutputDestination::Stdout,
        };
        let mut buffer = TextBuffer::from_text("piped commit message");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;

        let res = handle_key_press(
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(res, LoopControl::AcceptStdout);
    }

    #[test]
    fn test_stdout_chunk_streaming_parity() {
        let content = "line 1\nline 2 with some words\nline 3\n";
        let buffer = TextBuffer::from_text(content);
        let mut out = Vec::new();
        for chunk in buffer.chunks() {
            out.extend_from_slice(chunk.as_bytes());
        }
        assert_eq!(String::from_utf8(out).unwrap(), content);
    }

    #[test]
    fn test_from_bytes_bom_and_crlf_detection() {
        // Valid UTF-8 with BOM and CRLF
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice(b"first line\r\nsecond line\r\n");
        let buffer = TextBuffer::from_bytes(&raw).unwrap();
        assert_eq!(buffer.encoding(), karet_text::Encoding::Utf8Bom);
        assert_eq!(buffer.eol(), karet_text::Eol::Crlf);
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.line(0).unwrap(), "first line");
        assert_eq!(buffer.line(1).unwrap(), "second line");

        // Invalid UTF-8 rejected
        let invalid = [0xFF, 0xFE, 0xFD];
        assert!(TextBuffer::from_bytes(&invalid).is_err());
    }

    fn draw_editor_for_test(
        buffer: &TextBuffer,
        state: &mut EditorState,
        syntax: Option<&SyntaxState>,
        folded: &BTreeSet<u32>,
        width: u16,
        height: u16,
    ) -> (Rect, Rect) {
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::dark();
        let empty_folds = FoldRegions::default();
        let folds = syntax.map_or(&empty_folds, |syn| &syn.folds);
        let resolved_folds = resolve_folds(folds, folded);

        let render_ctx = RenderContext {
            buffer,
            theme: &theme,
            resolved_folds: &resolved_folds,
            highlights: syntax.and_then(|syn| syn.rendered_highlights.as_ref()),
            lang_name: "Rust",
            file_name: "test.rs",
            mode: EditorMode::Normal,
            in_confirm_discard: false,
        };

        let mut view_area = Rect::default();
        let mut ed_area = Rect::default();
        terminal
            .draw(|frame| {
                let (v, e) = render_editor(frame, &render_ctx, state);
                view_area = v;
                ed_area = e;
            })
            .unwrap();
        (view_area, ed_area)
    }

    #[test]
    fn test_click_tracker_streak_progression() {
        let mut tracker = ClickTracker::default();
        assert_eq!(tracker.register_click(10, 5), 1);
        assert_eq!(tracker.register_click(10, 5), 2);
        assert_eq!(tracker.register_click(11, 6), 3);
        // Streak in text caps at 3
        assert_eq!(tracker.register_click(11, 6), 3);

        tracker.reset();
        assert_eq!(tracker.register_click(10, 5), 1);
    }

    #[test]
    fn test_double_click_selects_word_and_drags_by_word() {
        let text = "fn calculate_total() -> u32 {\n    42\n}\n";
        let buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        let folded = BTreeSet::new();
        let (_, editor_area) = draw_editor_for_test(&buffer, &mut state, None, &folded, 80, 24);

        let empty_folds = FoldRegions::default();
        let resolved_folds = resolve_folds(&empty_folds, &folded);
        let mut folded_mut = folded;
        let mut auto_closers = Vec::new();
        let mut click_tracker = ClickTracker::default();
        let mut editor_drag = None;

        // Find screen column for `calculate_total`
        // In default editor gutter (line numbers on), gutter width is 4.
        // `fn calculate_total`: `fn ` is 3 chars, so `calculate_total` starts at buffer col 3 -> screen col 7.
        let target_col = 8;
        let target_row = 0;

        let mut ctx = MouseContext {
            editor_area,
            folds: &empty_folds,
            resolved_folds: &resolved_folds,
            folded: &mut folded_mut,
            auto_closers: &mut auto_closers,
            click_tracker: &mut click_tracker,
            editor_drag: &mut editor_drag,
        };

        // Click 1: caret placement
        let mouse_down_1 = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target_col,
            row: target_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(mouse_down_1, &buffer, &mut state, &mut ctx);
        assert!(
            state.selection_range().is_none()
                || state.selection_range().unwrap().start == state.selection_range().unwrap().end
        );

        // Click 2: word selection
        let mouse_down_2 = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target_col,
            row: target_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(mouse_down_2, &buffer, &mut state, &mut ctx);

        let sel = state.selection_range().expect("word should be selected");
        assert_eq!(sel.start, LineCol::new(0, 3));
        assert_eq!(sel.end, LineCol::new(0, 18));
        assert!(ctx.editor_drag.is_some());
        assert_eq!(ctx.editor_drag.unwrap().granularity, DragGranularity::Word);

        // Drag forward across `()` to `->`
        let mouse_drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: target_col + 15,
            row: target_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(mouse_drag, &buffer, &mut state, &mut ctx);
        let dragged_sel = state.selection_range().unwrap();
        assert_eq!(dragged_sel.start, LineCol::new(0, 3));
        assert!(dragged_sel.end.col > 18);

        // Mouse Up terminates drag
        let mouse_up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: target_col + 15,
            row: target_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(mouse_up, &buffer, &mut state, &mut ctx);
        assert!(ctx.editor_drag.is_none());
    }

    #[test]
    fn test_triple_click_selects_line_and_drags_by_line() {
        let text = "first line of code\nsecond line of code\nthird line of code\n";
        let buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        let folded = BTreeSet::new();
        let (_, editor_area) = draw_editor_for_test(&buffer, &mut state, None, &folded, 80, 24);

        let empty_folds = FoldRegions::default();
        let resolved_folds = resolve_folds(&empty_folds, &folded);
        let mut folded_mut = folded;
        let mut auto_closers = Vec::new();
        let mut click_tracker = ClickTracker::default();
        let mut editor_drag = None;

        let mut ctx = MouseContext {
            editor_area,
            folds: &empty_folds,
            resolved_folds: &resolved_folds,
            folded: &mut folded_mut,
            auto_closers: &mut auto_closers,
            click_tracker: &mut click_tracker,
            editor_drag: &mut editor_drag,
        };

        let target_col = 10;
        let target_row = 1; // second line

        // 3 consecutive clicks on line 1
        for _ in 0..3 {
            handle_mouse_event(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target_col,
                    row: target_row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &buffer,
                &mut state,
                &mut ctx,
            );
        }

        let sel = state.selection_range().expect("line should be selected");
        assert_eq!(sel.start, LineCol::new(1, 0));
        assert_eq!(sel.end, LineCol::new(2, 0));
        assert!(ctx.editor_drag.is_some());
        assert_eq!(ctx.editor_drag.unwrap().granularity, DragGranularity::Line);

        // Drag down to line 2 (third line)
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: target_col,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );
        let dragged_sel = state.selection_range().unwrap();
        assert_eq!(dragged_sel.start, LineCol::new(1, 0));
        assert_eq!(dragged_sel.end, LineCol::new(3, 0));

        // Release ends drag
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: target_col,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );
        assert!(ctx.editor_drag.is_none());
    }

    #[test]
    fn test_mouse_drag_character_selection_and_release() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        let folded = BTreeSet::new();
        let (_, editor_area) = draw_editor_for_test(&buffer, &mut state, None, &folded, 80, 24);

        let empty_folds = FoldRegions::default();
        let resolved_folds = resolve_folds(&empty_folds, &folded);
        let mut folded_mut = folded;
        let mut auto_closers = Vec::new();
        let mut click_tracker = ClickTracker::default();
        let mut editor_drag = None;

        let mut ctx = MouseContext {
            editor_area,
            folds: &empty_folds,
            resolved_folds: &resolved_folds,
            folded: &mut folded_mut,
            auto_closers: &mut auto_closers,
            click_tracker: &mut click_tracker,
            editor_drag: &mut editor_drag,
        };

        // Click at col 6 (text col 2)
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 6,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );
        assert!(ctx.editor_drag.is_some());
        assert_eq!(
            ctx.editor_drag.unwrap().granularity,
            DragGranularity::Character
        );

        // Drag to col 12
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 12,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );
        let sel = state.selection_range().expect("selection should exist");
        assert!(sel.start < sel.end);

        // Release
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 12,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );
        assert!(ctx.editor_drag.is_none());
    }

    #[test]
    fn test_gutter_fold_toggle_and_ellipsis_expand() {
        let text = "fn calculate() {\n    let a = 1;\n    let b = 2;\n}\n";
        let buffer = TextBuffer::from_text(text);
        let syntax = SyntaxState::new(languages::RUST.unwrap(), text);
        let mut state = EditorState::new();
        let mut folded = BTreeSet::new();

        let (_, editor_area) =
            draw_editor_for_test(&buffer, &mut state, Some(&syntax), &folded, 80, 24);
        let resolved_folds = resolve_folds(&syntax.folds, &folded);
        let mut auto_closers = Vec::new();
        let mut click_tracker = ClickTracker::default();
        let mut editor_drag = None;

        assert_ne!(syntax.folds.regions(), []);
        let fold_header_line = syntax.folds.regions()[0].start;
        assert_eq!(fold_header_line, 0);

        // Fold column in the gutter is column 2 (offset 2 where offset + 1 == gutter_width 3)
        let fold_gutter_col = 2;
        let mut ctx = MouseContext {
            editor_area,
            folds: &syntax.folds,
            resolved_folds: &resolved_folds,
            folded: &mut folded,
            auto_closers: &mut auto_closers,
            click_tracker: &mut click_tracker,
            editor_drag: &mut editor_drag,
        };

        // Click fold chevron in gutter
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: fold_gutter_col,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );

        // Line 0 is now collapsed!
        assert!(folded.contains(&0));

        // Redraw editor with collapsed fold so fold ellipsis `⋯` is painted
        draw_editor_for_test(&buffer, &mut state, Some(&syntax), &folded, 80, 24);
        let resolved_with_fold = resolve_folds(&syntax.folds, &folded);
        assert_eq!(resolved_with_fold.len(), 1);
        assert!(resolved_with_fold[0].collapsed);

        // Clicking ellipsis `⋯` expands the fold
        let ellipsis_hit = state.fold_ellipsis_at(20, 0);
        if let Some(line) = ellipsis_hit {
            assert_eq!(line, 0);
            let mut ctx_expand = MouseContext {
                editor_area,
                folds: &syntax.folds,
                resolved_folds: &resolved_with_fold,
                folded: &mut folded,
                auto_closers: &mut auto_closers,
                click_tracker: &mut click_tracker,
                editor_drag: &mut editor_drag,
            };
            handle_mouse_event(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 20,
                    row: 0,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &buffer,
                &mut state,
                &mut ctx_expand,
            );
            assert!(!folded.contains(&0));
        }
    }

    #[test]
    fn test_gutter_line_number_click_selects_line() {
        let text = "alpha\nbeta\ngamma\n";
        let buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        let folded = BTreeSet::new();
        let (_, editor_area) = draw_editor_for_test(&buffer, &mut state, None, &folded, 80, 24);

        let empty_folds = FoldRegions::default();
        let resolved_folds = resolve_folds(&empty_folds, &folded);
        let mut folded_mut = folded;
        let mut auto_closers = Vec::new();
        let mut click_tracker = ClickTracker::default();
        let mut editor_drag = None;

        let mut ctx = MouseContext {
            editor_area,
            folds: &empty_folds,
            resolved_folds: &resolved_folds,
            folded: &mut folded_mut,
            auto_closers: &mut auto_closers,
            click_tracker: &mut click_tracker,
            editor_drag: &mut editor_drag,
        };

        // Line number lane is typically column 1 or 2
        let line_num_col = 1;
        let line_num_row = 1; // "beta" line

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: line_num_col,
                row: line_num_row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &buffer,
            &mut state,
            &mut ctx,
        );

        let sel = state
            .selection_range()
            .expect("full line should be selected");
        assert_eq!(sel.start, LineCol::new(1, 0));
        assert_eq!(sel.end, LineCol::new(2, 0));
        assert!(ctx.editor_drag.is_some());
        assert_eq!(ctx.editor_drag.unwrap().granularity, DragGranularity::Line);
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

        // When initializing SyntaxState for plain text / language without fold query
        let syntax = SyntaxState::new(languages::PYTHON.unwrap(), text);
        assert_ne!(syntax.folds.regions(), []);
    }
}
