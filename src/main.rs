mod args;
mod editing;
mod keymap;
mod mouse;
mod status_bar;
mod syntax;
mod terminal;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use args::{CliArgs, OutputDestination, parse_args};
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use editing::handle_action;
use karet_core::{FoldRegions, Highlights, LineCol};
use karet_editor::{Editor, EditorState, Fold, resolve_folds};
use karet_filetype::file_type_for_path;
use karet_syntax::fold_by_indent;
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::language_id_from_path;
use keymap::{Action, EditorMode, map_key};
use mouse::{ClickTracker, EditorDrag, MouseContext, handle_drag_autoscroll, handle_mouse_event};
use ratatui::layout::{Position, Rect};
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

pub(crate) struct RenderContext<'a> {
    pub(crate) buffer: &'a TextBuffer,
    pub(crate) theme: &'a Theme,
    pub(crate) resolved_folds: &'a [Fold],
    pub(crate) highlights: Option<&'a Highlights>,
    pub(crate) lang_name: &'a str,
    pub(crate) file_name: &'a str,
    pub(crate) mode: EditorMode,
    pub(crate) in_confirm_discard: bool,
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
}
