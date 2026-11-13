use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use karet_core::{FoldRegions, LineCol};
use karet_editor::{EditorState, Fold, GutterLane};
use karet_text::{
    TextBuffer,
    words::{class_run_at, line_span},
};
use ratatui::layout::Rect;

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragGranularity {
    Character,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub struct EditorDrag {
    pub anchor: (LineCol, LineCol),
    pub granularity: DragGranularity,
    pub pointer: (u16, u16),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClickTracker {
    last_click: Option<(Instant, u16, u16)>,
    click_streak: u8,
}

impl ClickTracker {
    pub fn register_click(&mut self, col: u16, row: u16) -> u8 {
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

    pub const fn reset(&mut self) {
        self.last_click = None;
        self.click_streak = 0;
    }
}

#[must_use]
pub fn drag_span(
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

pub fn apply_drag_selection(
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

pub struct MouseContext<'a> {
    pub editor_area: Rect,
    pub folds: &'a FoldRegions,
    pub resolved_folds: &'a [Fold],
    pub folded: &'a mut BTreeSet<u32>,
    pub auto_closers: &'a mut Vec<LineCol>,
    pub click_tracker: &'a mut ClickTracker,
    pub editor_drag: &'a mut Option<EditorDrag>,
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

pub fn handle_mouse_event(
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

pub fn handle_drag_autoscroll(
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

#[cfg(test)]
mod tests {
    use karet_editor::resolve_folds;
    use karet_theme::Theme;
    use karet_treesitter::languages;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::app::{RenderContext, render_editor};
    use crate::keymap::EditorMode;
    use crate::syntax::SyntaxState;

    fn draw_editor_for_test(
        buffer: &TextBuffer,
        state: &mut EditorState,
        syntax: Option<&SyntaxState>,
        folded: &BTreeSet<u32>,
        width: u16,
        height: u16,
    ) -> (Rect, Rect) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::dark();
        let empty_folds = FoldRegions::default();
        let folds = syntax.map_or(&empty_folds, |syn| &syn.folds);
        let resolved_folds = resolve_folds(folds, folded);

        let search = crate::search::SearchState::default();
        let render_ctx = RenderContext {
            buffer,
            theme: &theme,
            resolved_folds: &resolved_folds,
            highlights: syntax.and_then(|syn| syn.rendered_highlights.as_ref()),
            decorations: &[],
            lang_name: "Rust",
            file_name: "test.rs",
            mode: EditorMode::Normal,
            in_confirm_discard: false,
            search: &search,
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
        let target_row = 1;

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

        assert!(folded.contains(&0));

        draw_editor_for_test(&buffer, &mut state, Some(&syntax), &folded, 80, 24);
        let resolved_with_fold = resolve_folds(&syntax.folds, &folded);
        assert_eq!(resolved_with_fold.len(), 1);
        assert!(resolved_with_fold[0].collapsed);

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

        let line_num_col = 1;
        let line_num_row = 1;

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
}
