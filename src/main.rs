mod keymap;
mod status_bar;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use karet_core::{FoldRegions, Highlights, LineCol, Range};
use karet_editor::{Editor, EditorState, Fold, editing, resolve_folds};
use karet_syntax::Highlighter;
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::{
    LanguageId, ParserPool, SyntaxTree, language_id_from_path, language_name_from_path,
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

struct SyntaxHighlighter {
    pool: ParserPool,
    highlighter: Highlighter,
    lang: LanguageId,
}

impl SyntaxHighlighter {
    fn new(lang: LanguageId) -> Option<Self> {
        let highlighter = Highlighter::new(lang).ok()?;
        Some(Self {
            pool: ParserPool::new(),
            highlighter,
            lang,
        })
    }

    fn analyze(&mut self, text: &str) -> (Option<Highlights>, FoldRegions) {
        if let Ok(tree) = SyntaxTree::parse(&mut self.pool, self.lang, text) {
            let highlights = self.highlighter.highlight(&tree, text).ok();
            let folds = karet_syntax::fold(&tree);
            (highlights, folds)
        } else {
            (None, FoldRegions::default())
        }
    }
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
        execute!(backend, EnableMouseCapture)?;
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
                let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
            } else if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                let mut backend = CrosstermBackend::new(tty);
                let _ = execute!(backend, DisableMouseCapture);
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

fn toggle_comment_on_current_line(
    buffer: &mut TextBuffer,
    state: &EditorState,
) -> anyhow::Result<()> {
    let line_idx = state.cursor().line;
    let line_str = buffer.line(line_idx as usize).unwrap_or_default();
    let trimmed = line_str.trim_start();
    let indent_len = u32::try_from(line_str.len() - trimmed.len()).unwrap_or(u32::MAX);

    if trimmed.starts_with("// ") {
        let range = Range {
            start: LineCol::new(line_idx, indent_len),
            end: LineCol::new(line_idx, indent_len.saturating_add(3)),
        };
        let edit = editing::insert(range.start, Some(range), buffer.version(), "");
        buffer.apply_simple(&edit.change)?;
    } else {
        let range = Range {
            start: LineCol::new(line_idx, indent_len),
            end: LineCol::new(line_idx, indent_len),
        };
        let edit = editing::insert(range.start, Some(range), buffer.version(), "// ");
        buffer.apply_simple(&edit.change)?;
    }
    Ok(())
}

fn handle_motion_action(action: Action, buffer: &TextBuffer, state: &mut EditorState) -> bool {
    match action {
        Action::MoveLeft { extend } => {
            state.move_left(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveRight { extend } => {
            state.move_right(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveUp { extend } => {
            state.move_up(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDown { extend } => {
            state.move_down(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveWordLeft { extend } => {
            state.move_word_left(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveWordRight { extend } => {
            state.move_word_right(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveLineStart { extend } => {
            state.move_line_start(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveLineEnd { extend } => {
            state.move_line_end(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDocStart { extend } => {
            state.move_doc_start(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDocEnd { extend } => {
            state.move_doc_end(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::PageUp { extend } => {
            state.page_up(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::PageDown { extend } => {
            state.page_down(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::SelectAll => {
            state.select_all(buffer);
            true
        }
        _ => false,
    }
}

fn handle_edit_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
) -> anyhow::Result<bool> {
    match action {
        Action::InsertChar(c) => {
            let edit = editing::insert(
                state.cursor(),
                state.selection_range(),
                buffer.version(),
                &c.to_string(),
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::InsertTab => {
            let edit = editing::insert(
                state.cursor(),
                state.selection_range(),
                buffer.version(),
                "    ",
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::InsertNewline => {
            let edit = editing::newline(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::DeleteBackward => {
            if let Some(edit) = editing::backspace(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteForward => {
            if let Some(edit) = editing::delete_forward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteWordBackward => {
            if let Some(edit) = editing::delete_word_backward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteWordForward => {
            if let Some(edit) = editing::delete_word_forward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn handle_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
) -> anyhow::Result<bool> {
    if handle_motion_action(action, buffer, state) {
        return Ok(false);
    }
    if handle_edit_action(action, buffer, state)? {
        return Ok(true);
    }

    match action {
        Action::Undo => {
            if let Some(applied) = buffer.undo() {
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::ToggleComment => {
            toggle_comment_on_current_line(buffer, state)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn handle_mouse_event(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    editor_area: Rect,
    resolved_folds: &[Fold],
    folded: &mut BTreeSet<u32>,
) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(line) = state.gutter_marker_line_at(
                editor_area,
                buffer,
                resolved_folds,
                mouse.column,
                mouse.row,
            ) {
                if !folded.remove(&line) {
                    folded.insert(line);
                }
            } else {
                let pos = state.pos_at(
                    editor_area,
                    buffer,
                    resolved_folds,
                    mouse.column,
                    mouse.row,
                );
                state.set_caret(buffer, pos);
            }
        }
        MouseEventKind::ScrollDown => {
            state.scroll_to_line(state.scroll_line.saturating_add(1));
        }
        MouseEventKind::ScrollUp => {
            state.scroll_to_line(state.scroll_line.saturating_sub(1));
        }
        _ => {}
    }
}

enum LoopControl {
    Continue { text_changed: bool },
    Exit(ExitCode),
}

fn handle_key_press(
    key: KeyEvent,
    in_confirm_discard: &mut bool,
    cli_args: &CliArgs,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
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
                buffer.save(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            Ok(LoopControl::Continue {
                text_changed: false,
            })
        }
        Action::SaveAndAccept => {
            match &cli_args.destination {
                OutputDestination::File(path) => {
                    buffer.save(path).map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                OutputDestination::Stdout => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(buffer.text().as_bytes())?;
                    stdout.flush()?;
                }
            }
            Ok(LoopControl::Exit(ExitCode::SUCCESS))
        }
        other => {
            let changed = handle_action(other, buffer, state)?;
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
    let bottom_line = (state.scroll_line as usize).saturating_add(top.height as usize);
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

                let mut input = String::new();
                std::io::stdin().read_to_string(&mut input)?;
                Ok((TextBuffer::from_text(&input), "[stdin]".to_owned()))
            }
        }
    }
}

fn run_editor_loop(
    guard: &mut TerminalGuard,
    cli_args: &CliArgs,
) -> anyhow::Result<ExitCode> {
    let (mut buffer, file_name) = init_buffer(&cli_args.destination)?;

    let (lang_id, lang_name) = match &cli_args.destination {
        OutputDestination::File(path) => (
            language_id_from_path(path),
            language_name_from_path(path).unwrap_or("Plain Text"),
        ),
        OutputDestination::Stdout => (None, "Plain Text"),
    };
    let mut syntax_highlighter = lang_id.and_then(SyntaxHighlighter::new);

    let mut state = EditorState::new();
    let theme = Theme::dark();
    let (mut highlights, mut folds) = syntax_highlighter
        .as_mut()
        .map_or_else(|| (None, FoldRegions::default()), |sh| sh.analyze(&buffer.text()));

    let mut folded: BTreeSet<u32> = BTreeSet::new();
    let mut viewport_area = Rect::default();
    let mut editor_area = Rect::default();
    let mut in_confirm_discard = false;

    loop {
        let resolved_folds = resolve_folds(&folds, &folded);

        let render_ctx = RenderContext {
            buffer: &buffer,
            theme: &theme,
            resolved_folds: &resolved_folds,
            highlights: highlights.as_ref(),
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

        if event::poll(std::time::Duration::from_millis(100))? {
            let mut text_changed = false;

            match event::read()? {
                Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                    match handle_key_press(
                        key,
                        &mut in_confirm_discard,
                        cli_args,
                        &mut buffer,
                        &mut state,
                    )? {
                        LoopControl::Continue {
                            text_changed: changed,
                        } => {
                            text_changed = changed;
                        }
                        LoopControl::Exit(code) => return Ok(code),
                    }
                }
                Event::Mouse(mouse) if !in_confirm_discard => {
                    handle_mouse_event(
                        mouse,
                        &buffer,
                        &mut state,
                        editor_area,
                        &resolved_folds,
                        &mut folded,
                    );
                }
                _ => {}
            }

            if text_changed
                && let Some(ref mut sh) = syntax_highlighter
            {
                let (new_highlights, new_folds) = sh.analyze(&buffer.text());
                highlights = new_highlights;
                folds = new_folds;
            }
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args = std::env::args();
    let Some(cli_args) = parse_args(args)? else {
        return Ok(ExitCode::SUCCESS);
    };

    let mut guard = TerminalGuard::init(10)?;
    let exit_code = run_editor_loop(&mut guard, &cli_args)?;
    Ok(exit_code)
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

        handle_action(Action::ToggleComment, &mut buffer, &mut state).unwrap();
        assert_eq!(buffer.text(), "// fn test() {}");

        handle_action(Action::ToggleComment, &mut buffer, &mut state).unwrap();
        assert_eq!(buffer.text(), "fn test() {}");
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
        assert_eq!(language_name_from_path(Path::new("test.rs")), Some("Rust"));
        assert!(language_id_from_path(Path::new("test.rs")).is_some());

        assert_eq!(
            language_name_from_path(Path::new("script.py")),
            Some("Python")
        );
        assert!(language_id_from_path(Path::new("script.py")).is_some());

        assert_eq!(
            language_name_from_path(Path::new("app.js")),
            Some("JavaScript")
        );
        assert!(language_id_from_path(Path::new("app.js")).is_some());

        assert_eq!(language_name_from_path(Path::new("main.go")), Some("Go"));
        assert!(language_id_from_path(Path::new("main.go")).is_some());

        assert_eq!(
            language_name_from_path(Path::new("README.md")),
            Some("Markdown")
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
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Continue { text_changed: false }));
        assert!(in_confirm_discard);

        // While in confirm discard, pressing 'n' cancels confirmation
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Continue { text_changed: false }));
        assert!(!in_confirm_discard);

        // Dirty Esc again -> confirm discard
        handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut in_confirm_discard,
            &cli_args,
            &mut buffer,
            &mut state,
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
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Exit(code) if code == ExitCode::SUCCESS));
    }
}
