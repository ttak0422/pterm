use crate::{Color, Parser, Screen};
use std::num::NonZeroU16;

fn full_text(screen: &Screen) -> String {
    let mut screen = screen.clone();
    let history = screen.scrollback_rows();
    let mut text = String::new();
    for index in 0..history {
        screen.set_scrollback(history - index);
        text.push_str(&screen.rows(0, u16::MAX).next().unwrap());
        if !screen.row_wrapped(0) {
            text.push('\n');
        }
    }
    screen.set_scrollback(0);
    for (index, row) in screen.rows(0, u16::MAX).enumerate() {
        text.push_str(&row);
        if !screen.row_wrapped(index as u16) {
            text.push('\n');
        }
    }
    text.trim_end_matches('\n').to_owned()
}

#[test]
fn history_and_live_text_survive_repeated_width_one_roundtrips() {
    let mut parser = Parser::new(
        NonZeroU16::new(4).unwrap(),
        NonZeroU16::new(20).unwrap(),
        100,
    );
    for line in 0..20 {
        parser.process(
            format!("line-{line:02}:abcdefghijklmnopqrstuvwxyz\r\n")
                .as_bytes(),
        );
    }
    parser.process(b"last-prompt> ");
    let text = full_text(parser.screen());
    let cursor = parser.screen().cursor_position();
    for (rows, cols) in [(4, 1), (1, 1), (9, 7), (4, 20), (2, 2), (4, 20)] {
        parser.screen_mut().set_size(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
        );
        assert_eq!(full_text(parser.screen()), text, "{rows}x{cols}");
    }
    assert_eq!(parser.screen().cursor_position(), cursor);
}

#[test]
fn wide_combining_and_styled_cells_survive_one_column_storage() {
    let mut parser = Parser::new(
        NonZeroU16::new(4).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
    );
    parser.process("\x1b[31;1mA界e\u{301}🙂Z\x1b[0m".as_bytes());
    let text = full_text(parser.screen());
    for width in [1, 2, 3, 1, 8] {
        parser.screen_mut().set_size(
            NonZeroU16::new(4).unwrap(),
            NonZeroU16::new(width).unwrap(),
        );
        assert_eq!(full_text(parser.screen()), text);
    }
    let screen = parser.screen();
    assert_eq!(screen.cell(0, 1).unwrap().contents(), "界");
    assert!(screen.cell(0, 1).unwrap().is_wide());
    assert!(screen.cell(0, 2).unwrap().is_wide_continuation());
    assert_eq!(screen.cell(0, 3).unwrap().contents(), "e\u{301}");
    assert_eq!(screen.cell(0, 3).unwrap().fgcolor(), Color::Idx(1));
    assert!(screen.cell(0, 3).unwrap().bold());
}

#[test]
fn one_by_one_accepts_new_wide_and_combining_output_without_panicking() {
    let mut parser = Parser::new(
        NonZeroU16::new(1).unwrap(),
        NonZeroU16::new(1).unwrap(),
        100,
    );
    parser.process("A界e\u{301}🙂Z".as_bytes());
    assert_eq!(full_text(parser.screen()), "A界e\u{301}🙂Z");
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(4).unwrap(), NonZeroU16::new(12).unwrap());
    assert_eq!(parser.screen().contents(), "A界e\u{301}🙂Z");
    assert!(parser.screen().cell(0, 1).unwrap().is_wide());
}

#[test]
fn cursor_and_saved_cursor_recover_after_becoming_hidden() {
    let mut parser = Parser::new(
        NonZeroU16::new(4).unwrap(),
        NonZeroU16::new(16).unwrap(),
        100,
    );
    parser.process(b"firstabcdefgh\r\nsecondabcdef\r\nthirdabcdef\x1b[1;3H\x1b7\x1b[2;5H");
    let text = full_text(parser.screen());
    let cursor = parser.screen().cursor_position();
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(1).unwrap());
    assert_eq!(full_text(parser.screen()), text);
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(4).unwrap(), NonZeroU16::new(16).unwrap());
    assert_eq!(parser.screen().cursor_position(), cursor);
    parser.process(b"\x1b8");
    assert_eq!(parser.screen().cursor_position(), (0, 2));
}

#[test]
fn resize_preserves_pending_parser_sequences_and_wrap_cursor() {
    let mut parser = Parser::new(
        NonZeroU16::new(4).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
    );
    parser.process(b"12345678\x1b[3");
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(4).unwrap(), NonZeroU16::new(1).unwrap());
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(4).unwrap(), NonZeroU16::new(8).unwrap());
    parser.process(b"1mX");
    assert_eq!(full_text(parser.screen()), "12345678X");
    assert_eq!(parser.screen().cell(1, 0).unwrap().fgcolor(), Color::Idx(1));
}

#[test]
fn alternate_resize_keeps_hidden_primary_and_primary_generation() {
    let mut parser = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(12).unwrap(),
        100,
    );
    parser.process(b"history-line\r\nlive-line-a\r\nlive-line-b\r\nprompt> ");
    let text = full_text(parser.screen());
    let generation = parser.screen().scrollback_generation();
    parser.process(b"\x1b[?1049hALTERNATE");
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(1).unwrap(), NonZeroU16::new(1).unwrap());
    parser.process("界X".as_bytes());
    assert_eq!(parser.screen().scrollback_generation(), generation);
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(3).unwrap(), NonZeroU16::new(12).unwrap());
    parser.process(b"\x1b[?1049l");
    assert_eq!(full_text(parser.screen()), text);
}

#[test]
fn width_one_does_not_turn_physical_wraps_into_history_eviction() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(16).unwrap(),
        5,
    );
    for line in 0..6 {
        parser.process(format!("{line:02}-abcdefghijk\r\n").as_bytes());
    }
    let text = full_text(parser.screen());
    let generation = parser.screen().scrollback_generation();
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(1).unwrap());
    assert!(parser.screen().scrollback_rows() > 5);
    assert_eq!(parser.screen().scrollback_generation(), generation);
    assert_eq!(full_text(parser.screen()), text);
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(16).unwrap());
    assert_eq!(full_text(parser.screen()), text);
    parser.process(b"new\r\n");
    assert!(parser.screen().scrollback_generation() > generation);
}

#[test]
fn history_is_bounded_even_for_one_unterminated_logical_line() {
    let mut parser = Parser::new(
        NonZeroU16::new(1).unwrap(),
        NonZeroU16::new(1).unwrap(),
        5,
    );
    parser.process(&vec![b'x'; 1000]);
    assert!(parser.screen().scrollback_rows() <= 5);
    assert_eq!(parser.screen().scrollback_generation(), 999);
    assert!(full_text(parser.screen()).len() <= 6);
}

#[test]
fn wide_padding_does_not_become_spaces_during_reflow() {
    let mut parser = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(3).unwrap(),
        100,
    );
    parser.process("ab界cd界".as_bytes());
    assert_eq!(full_text(parser.screen()), "ab界cd界");
    for width in [2, 1, 4, 12] {
        parser.screen_mut().set_size(
            NonZeroU16::new(3).unwrap(),
            NonZeroU16::new(width).unwrap(),
        );
        assert_eq!(full_text(parser.screen()), "ab界cd界");
    }
}

#[test]
fn a_full_history_budget_does_not_evict_only_because_of_resize() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(16).unwrap(),
        5,
    );
    for line in 0..20 {
        parser.process(format!("{line:02}-abcdefghijklm\r\n").as_bytes());
    }
    let text = full_text(parser.screen());
    for (rows, cols) in [(2, 1), (1, 1), (2, 16)] {
        parser.screen_mut().set_size(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
        );
        assert_eq!(full_text(parser.screen()), text);
    }
}

#[test]
fn reset_epoch_detects_reset_and_new_history_in_the_same_batch() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
    );
    parser.process(b"old-a\r\nold-b\r\nold-c");
    let archived = parser.screen().scrollback_generation();
    let epoch = parser.screen().reset_generation();
    parser.process(b"\x1bcnew-a\r\nnew-b\r\nnew-c\r\nnew-d\r\nnew-e");
    assert!(parser.screen().scrollback_generation() >= archived);
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
    assert!(!full_text(parser.screen()).contains("old-"));
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(1).unwrap(), NonZeroU16::new(1).unwrap());
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
}

#[test]
fn continuation_cell_cursor_offsets_recover_after_one_column_compression() {
    for saved in [false, true] {
        let mut parser = Parser::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU16::new(4).unwrap(),
            100,
        );
        parser.process("A界B\x1b[1;3H".as_bytes());
        assert_eq!(parser.screen().cursor_position(), (0, 2));
        if saved {
            parser.process(b"\x1b7\x1b[1;1H");
        }
        for (rows, cols) in [(2, 1), (1, 1), (2, 2), (2, 1), (2, 4)] {
            parser.screen_mut().set_size(
                NonZeroU16::new(rows).unwrap(),
                NonZeroU16::new(cols).unwrap(),
            );
        }
        if saved {
            parser.process(b"\x1b8");
        }
        assert_eq!(
            parser.screen().cursor_position(),
            (0, 2),
            "saved={saved}"
        );
        assert_eq!(full_text(parser.screen()), "A界B");
    }
}

#[test]
fn decawm_disabled_overwrites_the_right_margin_without_scrolling() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(5).unwrap(),
        100,
    );
    parser.process(b"\x1b[?7labcdef");
    assert!(!parser.screen().autowrap());
    assert_eq!(parser.screen().contents(), "abcdf");
    assert_eq!(parser.screen().cursor_position(), (0, 4));
    assert!(!parser.screen().row_wrapped(0));
    assert_eq!(parser.screen().scrollback_generation(), 0);
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(1).unwrap());
    parser
        .screen_mut()
        .set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(5).unwrap());
    parser.process(b"X");
    assert_eq!(parser.screen().contents(), "abcdX");
    parser.process(b"\x1b[?7hYZ");
    assert_eq!(parser.screen().contents(), "abcdYZ");
    assert!(parser.screen().row_wrapped(0));
}

#[test]
fn no_wrap_handles_wide_right_margin_and_one_column_combining_output() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(3).unwrap(),
        100,
    );
    parser.process("\x1b[?7lab界".as_bytes());
    assert_eq!(parser.screen().contents(), "ab界");
    assert_eq!(parser.screen().cell(0, 2).unwrap().natural_width(), 2);
    assert!(!parser.screen().cell(0, 2).unwrap().is_wide());
    assert!(!parser.screen().row_wrapped(0));
    parser.process(b"X");
    assert_eq!(parser.screen().contents(), "abX");
    assert_eq!(parser.screen().scrollback_generation(), 0);

    let mut narrow = Parser::new(
        NonZeroU16::new(1).unwrap(),
        NonZeroU16::new(1).unwrap(),
        100,
    );
    narrow.process("\x1b[?7lA界e\u{301}".as_bytes());
    assert_eq!(narrow.screen().contents(), "e\u{301}");
    assert_eq!(narrow.screen().cursor_position(), (0, 0));
    assert_eq!(narrow.screen().scrollback_generation(), 0);
}

#[test]
fn autowrap_mode_survives_snapshot_save_restore_and_resets_on_ris() {
    let mut parser = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(5).unwrap(),
        100,
    );
    parser.process(b"\x1b[?7labcdef\x1b7\x1b[?7h\x1b8");
    assert!(!parser.screen().autowrap());
    let mut replay = Parser::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(5).unwrap(),
        100,
    );
    replay.process(parser.screen().state_formatted().as_bytes());
    assert!(!replay.screen().autowrap());
    assert_eq!(replay.screen().contents(), "abcdf");
    replay.process(b"Z");
    assert_eq!(replay.screen().contents(), "abcdZ");
    parser.process(b"\x1bc");
    assert!(parser.screen().autowrap());
}

#[test]
fn upstream_vs16_cells_survive_reflow_and_snapshot() {
    let mut parser = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(12).unwrap(),
        100,
    );
    parser.process("A❤\u{fe0f}e\u{301}Z".as_bytes());
    let text = full_text(parser.screen());
    assert_eq!(parser.screen().cell(0, 1).unwrap().contents(), "❤\u{fe0f}");
    assert_eq!(parser.screen().cell(0, 1).unwrap().natural_width(), 2);
    for width in [5, 1, 2, 12] {
        parser.set_size(
            NonZeroU16::new(3).unwrap(),
            NonZeroU16::new(width).unwrap(),
        );
        assert_eq!(full_text(parser.screen()), text);
    }
    let mut replay = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(12).unwrap(),
        100,
    );
    replay.process(parser.screen().state_formatted().as_bytes());
    assert_eq!(replay.screen().contents(), parser.screen().contents());
    assert!(replay.screen().cell(0, 1).unwrap().is_wide());
}

#[test]
fn upstream_hvp_hpa_and_repeat_remain_enabled() {
    let mut parser = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(12).unwrap(),
        100,
    );
    parser.process(b"AB\x1b[2;3fQ\x1b[5GZ\x1b[2b");
    assert_eq!(parser.screen().contents(), "AB\n  Q ZZZ");
    let text = full_text(parser.screen());
    parser.set_size(NonZeroU16::new(3).unwrap(), NonZeroU16::new(1).unwrap());
    parser
        .set_size(NonZeroU16::new(3).unwrap(), NonZeroU16::new(12).unwrap());
    assert_eq!(full_text(parser.screen()), text);
}

#[derive(Default)]
struct ScrollCapture {
    rows: Vec<(String, bool)>,
}

impl crate::Callbacks for ScrollCapture {
    fn on_scroll(
        &mut self,
        row: crate::capture::RowContents<'_>,
        alternate: bool,
    ) {
        let mut formatted = String::new();
        row.write_formatted_basic(&mut formatted, &mut Default::default())
            .unwrap();
        self.rows.push((
            crate::capture::basic_formatted_to_plain(&formatted).collect(),
            alternate,
        ));
    }
}

#[test]
fn upstream_scroll_callback_reports_each_budget_eviction_once() {
    let mut parser = Parser::new_with_callbacks(
        NonZeroU16::new(1).unwrap(),
        NonZeroU16::new(4).unwrap(),
        1,
        ScrollCapture::default(),
    );
    parser.process(b"one\r\ntwo\r\nthree");
    let rows = &parser.callbacks().rows;
    assert_eq!(rows.iter().filter(|(text, _)| text == "one").count(), 1);
    assert!(rows.iter().all(|(_, alternate)| !alternate));
    let calls = rows.len();
    parser.set_size(NonZeroU16::new(1).unwrap(), NonZeroU16::new(1).unwrap());
    parser.set_size(NonZeroU16::new(1).unwrap(), NonZeroU16::new(4).unwrap());
    assert_eq!(
        parser.callbacks().rows.len(),
        calls,
        "reflow is not eviction"
    );
}

#[test]
fn upstream_clear_scrollback_invalidates_history_without_resurrection() {
    let mut parser = Parser::new_with_callbacks(
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
        ScrollCapture::default(),
    );
    parser.process(b"old-one\r\nold-two\r\nlive");
    assert_eq!(parser.screen().scrollback_rows(), 1);
    let epoch = parser.screen().reset_generation();
    parser.process(b"\x1b[3J");
    assert_eq!(parser.screen().scrollback_rows(), 0);
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
    assert_eq!(parser.callbacks().rows, vec![("old-one".to_owned(), false)]);
    parser.set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(1).unwrap());
    parser.set_size(NonZeroU16::new(2).unwrap(), NonZeroU16::new(8).unwrap());
    assert!(!full_text(parser.screen()).contains("old-one"));
    let epoch = parser.screen().reset_generation();
    parser.screen_mut().clear_scrollback();
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
}

#[test]
fn upstream_insert_delete_outside_scroll_region_are_noops() {
    let mut parser = Parser::new(
        NonZeroU16::new(4).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
    );
    parser.process(b"one\r\ntwo\r\nthree\r\nfour\x1b[2;3r\x1b[1;1H");
    let contents = parser.screen().contents();
    parser.process(b"\x1b[2L\x1b[2M");
    assert_eq!(parser.screen().contents(), contents);
    parser.process(b"\x1b[4;1H\x1b[2L\x1b[2M");
    assert_eq!(parser.screen().contents(), contents);
}

#[test]
fn upstream_resize_resets_scroll_region_and_history_offset() {
    let mut parser = Parser::new(
        NonZeroU16::new(3).unwrap(),
        NonZeroU16::new(8).unwrap(),
        100,
    );
    parser.process(b"one\r\ntwo\r\nthree\r\nfour\x1b[2;3r");
    parser.screen_mut().set_scrollback(1);
    parser.set_size(NonZeroU16::new(4).unwrap(), NonZeroU16::new(8).unwrap());
    assert_eq!(parser.screen().scrollback(), 0);
    parser.process(b"\x1b[4;1H\n");
    assert_eq!(parser.screen().cursor_position().0, 3);
    assert!(full_text(parser.screen()).contains("one"));
}
