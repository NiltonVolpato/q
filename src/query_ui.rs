use std::collections::BTreeSet;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use jaq_fmts::Format;
use jaq_json::Val;
use karet_core::{Highlights, ThemeRole};
use karet_editor::{Editor, EditorState, Fold, resolve_folds};
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::language_id_from_injection_name;
use ratatui::layout::{Constraint, Layout, Position, Rect};

use crate::app::{EditorOutcome, handle_view_or_search_action};
use crate::args::{CliArgs, OutputDestination};
use crate::editing;
use crate::keymap::{Action, EditorMode, map_key};
use crate::query::{QueryFocus, evaluate_query, format_name};
use crate::search::SearchState;
use crate::status_bar::{StatusBar, StatusBarConfig};
use crate::syntax::SyntaxState;
use crate::terminal::TerminalGuard;

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

pub fn run_query_loop(
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
                return Ok(EditorOutcome::Exit(std::process::ExitCode::from(1)));
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
