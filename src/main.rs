mod args;
mod editing;
mod keymap;
mod mouse;
mod query;
mod search;
mod status_bar;
mod syntax;
mod terminal;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use args::{CliArgs, OutputDestination, parse_args};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use editing::handle_action;
use jaq_fmts::Format;
use jaq_json::Val;
use karet_core::{Decoration, FoldRegions, Highlights, LineCol, ThemeRole};
use karet_editor::{Editor, EditorState, Fold, resolve_folds};
use karet_filetype::file_type_for_path;
use karet_syntax::fold_by_indent;
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::{LanguageId, language_id_from_injection_name, language_id_from_path};
use keymap::{Action, EditorMode, map_key};
use mouse::{ClickTracker, EditorDrag, MouseContext, handle_drag_autoscroll, handle_mouse_event};
use query::{QueryFocus, evaluate_query, format_name, parse_inputs};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use search::SearchState;
use status_bar::{StatusBar, StatusBarConfig};
use syntax::SyntaxState;
use terminal::{TerminalGuard, split_viewport};

fn save_buffer(buffer: &mut TextBuffer, path: &Path) -> anyhow::Result<()> {
    let result = if path.exists() {
        buffer.save(path)
    } else {
        buffer.save_new(path)
    };
    result.map(|_| ()).map_err(|err| match err {
        karet_text::TextError::Conflict => {
            anyhow::anyhow!(
                "conflict: file was modified externally on disk: {}",
                path.display()
            )
        }
        other => anyhow::anyhow!("{other}"),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LoopControl {
    Continue { text_changed: bool },
    Exit(ExitCode),
    AcceptStdout,
}

fn handle_search_key(
    key: KeyEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    search: &mut SearchState,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Esc => {
            search.close();
            search.query.clear();
            search.matches.clear();
            search.current = None;
        }
        KeyCode::Enter => {
            search.close();
        }
        KeyCode::Down => {
            if let Some(range) = search.next() {
                state.center_on(range.start.line);
            }
        }
        KeyCode::Up => {
            if let Some(range) = search.prev() {
                state.center_on(range.start.line);
            }
        }
        KeyCode::Char('n') if ctrl => {
            if let Some(range) = search.next() {
                state.center_on(range.start.line);
            }
        }
        KeyCode::Char('p') if ctrl => {
            if let Some(range) = search.prev() {
                state.center_on(range.start.line);
            }
        }
        KeyCode::Backspace => {
            search.query.pop();
            search.update_matches(buffer, state.scroll_line());
            if let Some(range) = search.current_range() {
                state.center_on(range.start.line);
            }
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            search.query.push(c);
            search.update_matches(buffer, state.scroll_line());
            if let Some(range) = search.current_range() {
                state.center_on(range.start.line);
            }
        }
        _ => {}
    }
}

struct KeyContext<'a> {
    in_confirm_discard: &'a mut bool,
    cli_args: &'a CliArgs,
    buffer: &'a mut TextBuffer,
    state: &'a mut EditorState,
    syntax_state: &'a mut Option<SyntaxState>,
    auto_closers: &'a mut Vec<LineCol>,
    search: &'a mut SearchState,
}

fn handle_key_press(key: KeyEvent, ctx: &mut KeyContext<'_>) -> anyhow::Result<LoopControl> {
    if *ctx.in_confirm_discard {
        return Ok(match key.code {
            KeyCode::Char('y' | 'Y') => LoopControl::Exit(ExitCode::from(1)),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                *ctx.in_confirm_discard = false;
                LoopControl::Continue {
                    text_changed: false,
                }
            }
            _ => LoopControl::Continue {
                text_changed: false,
            },
        });
    }

    if ctx.search.active {
        handle_search_key(key, ctx.buffer, ctx.state, ctx.search);
        return Ok(LoopControl::Continue {
            text_changed: false,
        });
    }

    let action = map_key(key, ctx.cli_args.mode);
    match action {
        Action::Abort => {
            if ctx.buffer.is_dirty() {
                *ctx.in_confirm_discard = true;
                Ok(LoopControl::Continue {
                    text_changed: false,
                })
            } else {
                Ok(LoopControl::Exit(ExitCode::from(1)))
            }
        }
        Action::Quit => {
            if ctx.buffer.is_dirty() {
                *ctx.in_confirm_discard = true;
                Ok(LoopControl::Continue {
                    text_changed: false,
                })
            } else {
                Ok(LoopControl::Exit(ExitCode::SUCCESS))
            }
        }
        Action::Save => {
            if let OutputDestination::File(path) = &ctx.cli_args.destination {
                save_buffer(ctx.buffer, path)?;
            }
            Ok(LoopControl::Continue {
                text_changed: false,
            })
        }
        Action::SaveAndAccept => match &ctx.cli_args.destination {
            OutputDestination::File(path) => {
                save_buffer(ctx.buffer, path)?;
                Ok(LoopControl::Exit(ExitCode::SUCCESS))
            }
            OutputDestination::Stdout => Ok(LoopControl::AcceptStdout),
        },
        Action::StartSearch
        | Action::SearchNext
        | Action::SearchPrev
        | Action::ScrollUp(_)
        | Action::ScrollDown(_) => {
            handle_view_or_search_action(action, ctx.buffer, ctx.state, ctx.search);
            Ok(LoopControl::Continue {
                text_changed: false,
            })
        }
        Action::PageDown { .. }
        | Action::PageUp { .. }
        | Action::MoveDocStart { .. }
        | Action::MoveDocEnd { .. }
            if ctx.cli_args.mode == EditorMode::View =>
        {
            handle_view_or_search_action(action, ctx.buffer, ctx.state, ctx.search);
            Ok(LoopControl::Continue {
                text_changed: false,
            })
        }
        other => {
            let changed = handle_action(
                other,
                ctx.buffer,
                ctx.state,
                &ctx.cli_args.destination,
                ctx.syntax_state,
                ctx.auto_closers,
            )?;
            Ok(LoopControl::Continue {
                text_changed: changed,
            })
        }
    }
}

fn handle_view_or_search_action(
    action: Action,
    buffer: &TextBuffer,
    state: &mut EditorState,
    search: &mut SearchState,
) {
    match action {
        Action::StartSearch => search.open(),
        Action::SearchNext => {
            if let Some(range) = search.next() {
                state.center_on(range.start.line);
            }
        }
        Action::SearchPrev => {
            if let Some(range) = search.prev() {
                state.center_on(range.start.line);
            }
        }
        Action::ScrollUp(n) => state.scroll_to_line(state.scroll_line().saturating_sub(n)),
        Action::ScrollDown(n) => state.scroll_to_line(state.scroll_line().saturating_add(n)),
        Action::PageDown { .. } => state.scroll_page_down(),
        Action::PageUp { .. } => state.scroll_page_up(),
        Action::MoveDocStart { .. } => state.scroll_to_line(0),
        Action::MoveDocEnd { .. } => {
            let last_line = u32::try_from(buffer.line_count().saturating_sub(1)).unwrap_or(u32::MAX);
            state.center_on(last_line);
        }
        _ => {}
    }
}

pub(crate) struct RenderContext<'a> {
    pub(crate) buffer: &'a TextBuffer,
    pub(crate) theme: &'a Theme,
    pub(crate) resolved_folds: &'a [Fold],
    pub(crate) highlights: Option<&'a Highlights>,
    pub(crate) decorations: &'a [Decoration],
    pub(crate) lang_name: &'a str,
    pub(crate) file_name: &'a str,
    pub(crate) mode: EditorMode,
    pub(crate) in_confirm_discard: bool,
    pub(crate) search: &'a SearchState,
}

pub(crate) fn render_editor(
    frame: &mut ratatui::Frame,
    ctx: &RenderContext,
    state: &mut EditorState,
) -> (Rect, Rect) {
    let area = frame.area();
    let (top, bottom) = split_viewport(area);

    let mut editor_widget = Editor::new(ctx.buffer)
        .theme(ctx.theme)
        .folds(ctx.resolved_folds)
        .decorations(ctx.decorations)
        .read_only(ctx.mode == EditorMode::View)
        .word_wrap(true)
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
        search: Some(ctx.search),
        query_focus: None,
        query_error: None,
    };
    StatusBar::new(ctx.theme, &status_config).draw(frame, bottom);

    if ctx.search.active {
        let query_len = u16::try_from(ctx.search.query.chars().count()).unwrap_or(u16::MAX);
        let max_x = bottom.right().saturating_sub(1);
        let cursor_x = bottom.x.saturating_add(1).saturating_add(query_len).min(max_x);
        frame.set_cursor_position(Position::new(cursor_x, bottom.y));
    } else if ctx.mode != EditorMode::View
        && let Some((x, y)) = state.primary_caret_cell(top, ctx.buffer, ctx.resolved_folds)
    {
        frame.set_cursor_position(Position::new(x, y));
    }

    (area, top)
}

fn init_raw_input(
    destination: &OutputDestination,
) -> anyhow::Result<(Vec<u8>, String, Option<PathBuf>)> {
    match destination {
        OutputDestination::File(path) => {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_owned();
            let bytes =
                std::fs::read(path).map_err(|e| anyhow::anyhow!("failed to read file: {e}"))?;
            Ok((bytes, name, Some(path.clone())))
        }
        OutputDestination::Stdout => {
            use std::io::{IsTerminal as _, Read as _};

            if std::io::stdin().is_terminal() {
                anyhow::bail!("no file specified and stdin is a terminal.");
            }
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            Ok((input, "[stdin]".to_owned(), None))
        }
    }
}

fn init_buffer(
    destination: &OutputDestination,
    mode: EditorMode,
) -> anyhow::Result<(TextBuffer, String)> {
    match destination {
        OutputDestination::File(path) => {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_owned();

            let buf = if path.exists() {
                TextBuffer::load(path).map_err(|e| anyhow::anyhow!("failed to read file: {e}"))?
            } else {
                TextBuffer::from_text("")
            };
            Ok((buf, name))
        }
        OutputDestination::Stdout => {
            use std::io::IsTerminal as _;

            if std::io::stdin().is_terminal() {
                if mode == EditorMode::View {
                    anyhow::bail!("no file specified and stdin is a terminal.");
                }
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

fn resolve_language(cli_args: &CliArgs) -> (Option<LanguageId>, &'static str) {
    if let Some(lang) = cli_args.language.as_deref() {
        let dummy = PathBuf::from(format!("file.{lang}"));
        let lang_id =
            language_id_from_injection_name(lang).or_else(|| language_id_from_path(&dummy));
        let ft = file_type_for_path(&dummy);
        let lang_name = if ft.is_recognized() {
            ft.name()
        } else {
            "Plain Text"
        };
        return (lang_id, lang_name);
    }
    match &cli_args.destination {
        OutputDestination::File(path) => {
            (language_id_from_path(path), file_type_for_path(path).name())
        }
        OutputDestination::Stdout => (None, "Plain Text"),
    }
}

enum EditorOutcome {
    AcceptStdout(TextBuffer),
    Exit(ExitCode),
}

fn run_editor_loop(
    guard: &mut TerminalGuard,
    cli_args: &CliArgs,
    mut buffer: TextBuffer,
    file_name: &str,
) -> anyhow::Result<EditorOutcome> {
    let (lang_id, lang_name) = resolve_language(cli_args);
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
    let mut search = SearchState::default();

    loop {
        let folds = syntax_state.as_ref().map_or(&plain_folds, |syn| &syn.folds);
        let resolved_folds = resolve_folds(folds, &folded);
        let search_decorations = search.decorations();

        let render_ctx = RenderContext {
            buffer: &buffer,
            theme: &theme,
            resolved_folds: &resolved_folds,
            highlights: syntax_state
                .as_ref()
                .and_then(|syn| syn.rendered_highlights.as_ref()),
            decorations: &search_decorations,
            lang_name,
            file_name,
            mode: cli_args.mode,
            in_confirm_discard,
            search: &search,
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
                    let mut key_ctx = KeyContext {
                        in_confirm_discard: &mut in_confirm_discard,
                        cli_args,
                        buffer: &mut buffer,
                        state: &mut state,
                        syntax_state: &mut syntax_state,
                        auto_closers: &mut auto_closers,
                        search: &mut search,
                    };
                    match handle_key_press(key, &mut key_ctx)? {
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

struct QueryRenderContext<'a> {
    preview_buf: &'a TextBuffer,
    preview_highlights: Option<&'a Highlights>,
    preview_folds: &'a [Fold],
    query_buf: &'a TextBuffer,
    focus: QueryFocus,
    query_error: Option<&'a str>,
    null_suppressed: bool,
    file_name: &'a str,
    lang_name: &'a str,
    theme: &'a Theme,
}

fn line_number_digits(line_count: usize) -> u16 {
    let mut n = line_count.max(1);
    let mut digits = 0u16;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits
}

fn render_focus_bar(frame: &mut ratatui::Frame, area: Rect, focused: bool, theme: &Theme) {
    let bg = theme.role(ThemeRole::Background).to_ratatui();
    let fg = theme.role(ThemeRole::Cursor).to_ratatui();
    let (ch, style) = if focused {
        (
            "\u{258c}",
            ratatui::style::Style::default().fg(fg).bg(bg),
        )
    } else {
        (" ", ratatui::style::Style::default().bg(bg))
    };
    let buf = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_symbol(ch).set_style(style);
        }
    }
}

fn render_query_separator(
    frame: &mut ratatui::Frame,
    area: Rect,
    query_error: Option<&str>,
    null_suppressed: bool,
    theme: &Theme,
) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let bg = theme.role(ThemeRole::StatusBarBackground).to_ratatui();
    let line = query_error.map_or_else(
        || {
            let muted = theme.role(ThemeRole::StatusBarForeground).to_ratatui();
            if null_suppressed {
                Line::from(Span::styled(
                    " (null \u{2014} keeping previous)",
                    Style::default().fg(muted).bg(bg),
                ))
            } else {
                Line::from("")
            }
        },
        |err| {
            let warn = theme.role(ThemeRole::DiagnosticWarning).to_ratatui();
            Line::from(Span::styled(
                format!(" \u{26a0} {err}"),
                Style::default().fg(warn).bg(bg).add_modifier(Modifier::BOLD),
            ))
        },
    );
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
}

fn render_query(
    frame: &mut ratatui::Frame,
    ctx: &QueryRenderContext<'_>,
    preview_state: &mut EditorState,
    query_state: &mut EditorState,
) -> Rect {
    let area = frame.area();
    let query_height = u16::try_from(ctx.query_buf.line_count().clamp(1, 4)).unwrap_or(4);
    let [top, sep, mid, bottom] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(query_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let [top_bar, top_editor] =
        Layout::horizontal([Constraint::Length(1), Constraint::Fill(1)]).areas(top);
    render_focus_bar(frame, top_bar, ctx.focus == QueryFocus::Output, ctx.theme);

    let mut preview_widget = Editor::new(ctx.preview_buf)
        .theme(ctx.theme)
        .folds(ctx.preview_folds)
        .read_only(true)
        .word_wrap(true)
        .focused(ctx.focus == QueryFocus::Output);
    if let Some(h) = ctx.preview_highlights {
        preview_widget = preview_widget.highlights(h);
    }
    frame.render_stateful_widget(preview_widget, top_editor, preview_state);

    render_query_separator(frame, sep, ctx.query_error, ctx.null_suppressed, ctx.theme);

    let [mid_bar, mid_rest] =
        Layout::horizontal([Constraint::Length(1), Constraint::Fill(1)]).areas(mid);
    render_focus_bar(frame, mid_bar, ctx.focus == QueryFocus::Query, ctx.theme);

    let align_pad = line_number_digits(ctx.preview_buf.line_count())
        .saturating_sub(line_number_digits(ctx.query_buf.line_count()));
    let [mid_pad, mid_editor] =
        Layout::horizontal([Constraint::Length(align_pad), Constraint::Fill(1)]).areas(mid_rest);
    if align_pad > 0 {
        render_focus_bar(frame, mid_pad, false, ctx.theme);
    }

    let query_widget = Editor::new(ctx.query_buf)
        .theme(ctx.theme)
        .word_wrap(true)
        .focused(ctx.focus == QueryFocus::Query);
    frame.render_stateful_widget(query_widget, mid_editor, query_state);

    let total_lines = ctx.preview_buf.line_count();
    let bottom_line =
        (preview_state.scroll_line() as usize).saturating_add(top_editor.height as usize);
    let pct_below = (total_lines.saturating_sub(bottom_line) * 100)
        .checked_div(total_lines)
        .and_then(|pct| u8::try_from(pct).ok())
        .unwrap_or(0);

    let status_config = StatusBarConfig {
        cursor: preview_state.cursor(),
        selection: preview_state.selection_range(),
        language: ctx.lang_name,
        file_name: ctx.file_name,
        is_dirty: false,
        mode: EditorMode::Query,
        in_confirm_discard: false,
        pct_below,
        search: None,
        query_focus: Some(ctx.focus),
        query_error: None,
    };
    StatusBar::new(ctx.theme, &status_config).draw(frame, bottom);

    if ctx.focus == QueryFocus::Query
        && let Some((x, y)) = query_state.primary_caret_cell(mid_editor, ctx.query_buf, &[])
    {
        frame.set_cursor_position(Position::new(x, y));
    }

    area
}

fn refresh_query_preview(
    inputs: &[Val],
    query: &str,
    preview_buf: &mut TextBuffer,
    preview_state: &mut EditorState,
    preview_syntax: &mut Option<SyntaxState>,
    query_error: &mut Option<String>,
    null_suppressed: &mut bool,
) {
    match evaluate_query(inputs, query) {
        Ok(out) => {
            *query_error = None;
            if out.trim() == "null" && query.trim() != "null" {
                *null_suppressed = true;
            } else {
                *null_suppressed = false;
                *preview_buf = TextBuffer::from_text(&out);
                *preview_state = EditorState::new();
                if let Some(syn) = preview_syntax {
                    syn.update(&out);
                }
            }
        }
        Err(err) => {
            *query_error = Some(err);
        }
    }
}

fn init_query_editor(initial_query: &str) -> anyhow::Result<(TextBuffer, EditorState)> {
    let mut buf = TextBuffer::from_text(initial_query);
    let mut state = EditorState::new();
    let _ = editing::handle_action(
        Action::MoveDocEnd { extend: false },
        &mut buf,
        &mut state,
        &OutputDestination::Stdout,
        &mut None,
        &mut Vec::new(),
    )?;
    Ok((buf, state))
}

fn run_query_loop(
    guard: &mut TerminalGuard,
    cli_args: &CliArgs,
    inputs: &[Val],
    fmt: Format,
    file_name: &str,
) -> anyhow::Result<EditorOutcome> {
    let initial_query = cli_args.query.clone().unwrap_or_else(|| ".".to_owned());
    let (mut query_buf, mut query_state) = init_query_editor(&initial_query)?;
    let mut query_auto_closers = Vec::new();
    let mut query_syntax: Option<SyntaxState> = None;

    let (initial_out, mut query_error) = match evaluate_query(inputs, &initial_query) {
        Ok(out) => (out, None),
        Err(err) => (String::new(), Some(err)),
    };
    let mut preview_buf = TextBuffer::from_text(&initial_out);
    let mut preview_state = EditorState::new();
    let json_lang = language_id_from_injection_name("json");
    let mut preview_syntax = json_lang.map(|lang| SyntaxState::new(lang, &preview_buf.text()));
    let theme = Theme::dark();
    let mut focus = QueryFocus::Query;
    let mut null_suppressed = false;
    let lang_name = format_name(fmt);
    let mut viewport_area = Rect::default();

    loop {
        let preview_folds = preview_syntax
            .as_ref()
            .map_or(Vec::new(), |syn| resolve_folds(&syn.folds, &BTreeSet::new()));

        let render_ctx = QueryRenderContext {
            preview_buf: &preview_buf,
            preview_highlights: preview_syntax
                .as_ref()
                .and_then(|syn| syn.rendered_highlights.as_ref()),
            preview_folds: &preview_folds,
            query_buf: &query_buf,
            focus,
            query_error: query_error.as_deref(),
            null_suppressed,
            file_name,
            lang_name,
            theme: &theme,
        };
        guard.terminal_mut().draw(|frame| {
            viewport_area = render_query(frame, &render_ctx, &mut preview_state, &mut query_state);
        })?;
        guard.set_viewport_area(viewport_area);

        if event::poll(std::time::Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
                focus = focus.toggle();
                continue;
            }
            if key.code == KeyCode::Esc
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'q')))
            {
                return Ok(EditorOutcome::Exit(ExitCode::from(1)));
            }
            if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
                let selected = match focus {
                    QueryFocus::Query => query_buf,
                    QueryFocus::Output => preview_buf,
                };
                return Ok(EditorOutcome::AcceptStdout(selected));
            }

            match focus {
                QueryFocus::Query => {
                    let action = map_key(key, EditorMode::Prompt);
                    if editing::handle_action(
                        action,
                        &mut query_buf,
                        &mut query_state,
                        &OutputDestination::Stdout,
                        &mut query_syntax,
                        &mut query_auto_closers,
                    )? {
                        refresh_query_preview(
                            inputs,
                            &query_buf.text(),
                            &mut preview_buf,
                            &mut preview_state,
                            &mut preview_syntax,
                            &mut query_error,
                            &mut null_suppressed,
                        );
                    }
                }
                QueryFocus::Output => {
                    let action = map_key(key, EditorMode::View);
                    let mut dummy_search = SearchState::default();
                    handle_view_or_search_action(
                        action,
                        &preview_buf,
                        &mut preview_state,
                        &mut dummy_search,
                    );
                }
            }
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args = std::env::args();
    let Some(cli_args) = parse_args(args)? else {
        return Ok(ExitCode::SUCCESS);
    };

    let outcome = if cli_args.mode == EditorMode::Query {
        let (raw_bytes, file_name, path_opt) = init_raw_input(&cli_args.destination)?;
        let (inputs, fmt) =
            parse_inputs(&raw_bytes, path_opt.as_deref(), cli_args.format.as_deref())?;
        let mut guard = TerminalGuard::init(12)?;
        let outcome = run_query_loop(&mut guard, &cli_args, &inputs, fmt, &file_name)?;
        drop(guard);
        outcome
    } else {
        let (buffer, file_name) = init_buffer(&cli_args.destination, cli_args.mode)?;
        let mut guard = TerminalGuard::init(10)?;
        let outcome = run_editor_loop(&mut guard, &cli_args, buffer, &file_name)?;
        drop(guard);
        outcome
    };

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
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use karet_core::LineCol;
    use karet_editor::editing;

    use super::*;

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
            language: None,
            query: None,
            format: None,
        };
        let mut buffer = TextBuffer::from_text("clean text");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;
        let mut search = SearchState::default();

        let mut key_ctx = KeyContext {
            in_confirm_discard: &mut in_confirm_discard,
            cli_args: &cli_args,
            buffer: &mut buffer,
            state: &mut state,
            syntax_state: &mut None,
            auto_closers: &mut Vec::new(),
            search: &mut search,
        };

        // Clean Esc: exits immediately with code 1
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(matches!(res, LoopControl::Exit(code) if code == ExitCode::from(1)));
        assert!(!*key_ctx.in_confirm_discard);

        // Edit buffer -> dirty
        let edit = editing::insert(LineCol::new(0, 0), None, key_ctx.buffer.version(), "dirty ");
        key_ctx.buffer.apply_simple(&edit.change).unwrap();
        assert!(key_ctx.buffer.is_dirty());

        // Dirty Esc: enters confirmation mode
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(matches!(
            res,
            LoopControl::Continue {
                text_changed: false
            }
        ));
        assert!(*key_ctx.in_confirm_discard);

        // While in confirm discard, pressing 'n' cancels confirmation
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(matches!(
            res,
            LoopControl::Continue {
                text_changed: false
            }
        ));
        assert!(!*key_ctx.in_confirm_discard);

        // Dirty Esc again -> confirm discard
        handle_key_press(
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(*key_ctx.in_confirm_discard);

        // Pressing 'y' confirms discard and exits with code 1
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('y'), crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
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
            language: None,
            query: None,
            format: None,
        };
        let mut buffer = TextBuffer::from_text("commit message");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;
        let mut search = SearchState::default();

        let mut key_ctx = KeyContext {
            in_confirm_discard: &mut in_confirm_discard,
            cli_args: &cli_args,
            buffer: &mut buffer,
            state: &mut state,
            syntax_state: &mut None,
            auto_closers: &mut Vec::new(),
            search: &mut search,
        };

        // In prompt mode, Enter saves and accepts with code 0
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
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
            language: None,
            query: None,
            format: None,
        };
        let mut buffer = TextBuffer::from_text("piped commit message");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;
        let mut search = SearchState::default();

        let mut key_ctx = KeyContext {
            in_confirm_discard: &mut in_confirm_discard,
            cli_args: &cli_args,
            buffer: &mut buffer,
            state: &mut state,
            syntax_state: &mut None,
            auto_closers: &mut Vec::new(),
            search: &mut search,
        };

        let res = handle_key_press(
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert_eq!(res, LoopControl::AcceptStdout);
    }

    #[test]
    fn test_handle_key_press_view_mode_quit_and_search() {
        let cli_args = CliArgs {
            mode: EditorMode::View,
            destination: OutputDestination::Stdout,
            language: Some("rust".to_owned()),
            query: None,
            format: None,
        };
        let mut buffer = TextBuffer::from_text("fn main() {\n    println!(\"hi\");\n}\n");
        let mut state = EditorState::new();
        let mut in_confirm_discard = false;
        let mut search = SearchState::default();
        let mut syntax_state = None;
        let mut auto_closers = Vec::new();
        let mut key_ctx = KeyContext {
            in_confirm_discard: &mut in_confirm_discard,
            cli_args: &cli_args,
            buffer: &mut buffer,
            state: &mut state,
            syntax_state: &mut syntax_state,
            auto_closers: &mut auto_closers,
            search: &mut search,
        };

        // '/' activates search bar
        handle_key_press(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(key_ctx.search.active);

        // Typing 'main' finds match
        for ch in "main".chars() {
            handle_key_press(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut key_ctx,
            )
            .unwrap();
        }
        assert_eq!(key_ctx.search.matches.len(), 1);

        // Enter closes search input bar while keeping matches
        handle_key_press(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert!(!key_ctx.search.active);
        assert_eq!(key_ctx.search.matches.len(), 1);

        // 'q' quits cleanly
        let res = handle_key_press(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut key_ctx,
        )
        .unwrap();
        assert_eq!(res, LoopControl::Exit(ExitCode::SUCCESS));
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
}
