use crate::{Color, Parser, Screen};

fn full_text(screen: &Screen) -> String {
    let mut screen = screen.clone();
    let history = screen.scrollback_rows();
    let mut text = String::new();
    for index in 0..history {
        screen.set_scrollback(history - index);
        text.push_str(&screen.rows(0, u16::MAX).next().unwrap());
        if !screen.row_wrapped(0) { text.push('\n'); }
    }
    screen.set_scrollback(0);
    for (index, row) in screen.rows(0, u16::MAX).enumerate() {
        text.push_str(&row);
        if !screen.row_wrapped(index as u16) { text.push('\n'); }
    }
    text.trim_end_matches('\n').to_owned()
}

#[test]
fn history_and_live_text_survive_repeated_width_one_roundtrips() {
    let mut parser = Parser::new(4, 20, 100);
    for line in 0..20 {
        parser.process(format!("line-{line:02}:abcdefghijklmnopqrstuvwxyz\r\n").as_bytes());
    }
    parser.process(b"last-prompt> ");
    let text = full_text(parser.screen());
    let cursor = parser.screen().cursor_position();
    for (rows, cols) in [(4, 1), (1, 1), (9, 7), (4, 20), (2, 2), (4, 20)] {
        parser.screen_mut().set_size(rows, cols);
        assert_eq!(full_text(parser.screen()), text, "{rows}x{cols}");
    }
    assert_eq!(parser.screen().cursor_position(), cursor);
}

#[test]
fn wide_combining_and_styled_cells_survive_one_column_storage() {
    let mut parser = Parser::new(4, 8, 100);
    parser.process("\x1b[31;1mA界e\u{301}🙂Z\x1b[0m".as_bytes());
    let text = full_text(parser.screen());
    for width in [1, 2, 3, 1, 8] {
        parser.screen_mut().set_size(4, width);
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
    let mut parser = Parser::new(1, 1, 100);
    parser.process("A界e\u{301}🙂Z".as_bytes());
    assert_eq!(full_text(parser.screen()), "A界e\u{301}🙂Z");
    parser.screen_mut().set_size(4, 12);
    assert_eq!(parser.screen().contents(), "A界e\u{301}🙂Z");
    assert!(parser.screen().cell(0, 1).unwrap().is_wide());
}

#[test]
fn cursor_and_saved_cursor_recover_after_becoming_hidden() {
    let mut parser = Parser::new(4, 16, 100);
    parser.process(b"firstabcdefgh\r\nsecondabcdef\r\nthirdabcdef\x1b[1;3H\x1b7\x1b[2;5H");
    let text = full_text(parser.screen());
    let cursor = parser.screen().cursor_position();
    parser.screen_mut().set_size(2, 1);
    assert_eq!(full_text(parser.screen()), text);
    parser.screen_mut().set_size(4, 16);
    assert_eq!(parser.screen().cursor_position(), cursor);
    parser.process(b"\x1b8");
    assert_eq!(parser.screen().cursor_position(), (0, 2));
}

#[test]
fn resize_preserves_pending_parser_sequences_and_wrap_cursor() {
    let mut parser = Parser::new(4, 8, 100);
    parser.process(b"12345678\x1b[3");
    parser.screen_mut().set_size(4, 1);
    parser.screen_mut().set_size(4, 8);
    parser.process(b"1mX");
    assert_eq!(full_text(parser.screen()), "12345678X");
    assert_eq!(parser.screen().cell(1, 0).unwrap().fgcolor(), Color::Idx(1));
}

#[test]
fn alternate_resize_keeps_hidden_primary_and_primary_generation() {
    let mut parser = Parser::new(3, 12, 100);
    parser.process(b"history-line\r\nlive-line-a\r\nlive-line-b\r\nprompt> ");
    let text = full_text(parser.screen());
    let generation = parser.screen().scrollback_generation();
    parser.process(b"\x1b[?1049hALTERNATE");
    parser.screen_mut().set_size(1, 1);
    parser.process("界X".as_bytes());
    assert_eq!(parser.screen().scrollback_generation(), generation);
    parser.screen_mut().set_size(3, 12);
    parser.process(b"\x1b[?1049l");
    assert_eq!(full_text(parser.screen()), text);
}

#[test]
fn width_one_does_not_turn_physical_wraps_into_history_eviction() {
    let mut parser = Parser::new(2, 16, 5);
    for line in 0..6 {
        parser.process(format!("{line:02}-abcdefghijk\r\n").as_bytes());
    }
    let text = full_text(parser.screen());
    let generation = parser.screen().scrollback_generation();
    parser.screen_mut().set_size(2, 1);
    assert!(parser.screen().scrollback_rows() > 5);
    assert_eq!(parser.screen().scrollback_generation(), generation);
    assert_eq!(full_text(parser.screen()), text);
    parser.screen_mut().set_size(2, 16);
    assert_eq!(full_text(parser.screen()), text);
    parser.process(b"new\r\n");
    assert!(parser.screen().scrollback_generation() > generation);
}

#[test]
fn history_is_bounded_even_for_one_unterminated_logical_line() {
    let mut parser = Parser::new(1, 1, 5);
    parser.process(&vec![b'x'; 1000]);
    assert!(parser.screen().scrollback_rows() <= 5);
    assert_eq!(parser.screen().scrollback_generation(), 999);
    assert!(full_text(parser.screen()).len() <= 6);
}

#[test]
fn wide_padding_does_not_become_spaces_during_reflow() {
    let mut parser = Parser::new(3, 3, 100);
    parser.process("ab界cd界".as_bytes());
    assert_eq!(full_text(parser.screen()), "ab界cd界");
    for width in [2, 1, 4, 12] {
        parser.screen_mut().set_size(3, width);
        assert_eq!(full_text(parser.screen()), "ab界cd界");
    }
}

#[test]
fn a_full_history_budget_does_not_evict_only_because_of_resize() {
    let mut parser = Parser::new(2, 16, 5);
    for line in 0..20 {
        parser.process(format!("{line:02}-abcdefghijklm\r\n").as_bytes());
    }
    let text = full_text(parser.screen());
    for (rows, cols) in [(2, 1), (1, 1), (2, 16)] {
        parser.screen_mut().set_size(rows, cols);
        assert_eq!(full_text(parser.screen()), text);
    }
}

#[test]
fn reset_epoch_detects_reset_and_new_history_in_the_same_batch() {
    let mut parser = Parser::new(2, 8, 100);
    parser.process(b"old-a\r\nold-b\r\nold-c");
    let archived = parser.screen().scrollback_generation();
    let epoch = parser.screen().reset_generation();
    parser.process(b"\x1bcnew-a\r\nnew-b\r\nnew-c\r\nnew-d\r\nnew-e");
    assert!(parser.screen().scrollback_generation() >= archived);
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
    assert!(!full_text(parser.screen()).contains("old-"));
    parser.screen_mut().set_size(1, 1);
    assert_eq!(parser.screen().reset_generation(), epoch + 1);
}

#[test]
fn continuation_cell_cursor_offsets_recover_after_one_column_compression() {
    for saved in [false, true] {
        let mut parser = Parser::new(2, 4, 100);
        parser.process("A界B\x1b[1;3H".as_bytes());
        assert_eq!(parser.screen().cursor_position(), (0, 2));
        if saved { parser.process(b"\x1b7\x1b[1;1H"); }
        for (rows, cols) in [(2, 1), (1, 1), (2, 2), (2, 1), (2, 4)] {
            parser.screen_mut().set_size(rows, cols);
        }
        if saved { parser.process(b"\x1b8"); }
        assert_eq!(parser.screen().cursor_position(), (0, 2), "saved={saved}");
        assert_eq!(full_text(parser.screen()), "A界B");
    }
}

#[test]
fn decawm_disabled_overwrites_the_right_margin_without_scrolling() {
    let mut parser = Parser::new(2, 5, 100);
    parser.process(b"\x1b[?7labcdef");
    assert!(!parser.screen().autowrap());
    assert_eq!(parser.screen().contents(), "abcdf");
    assert_eq!(parser.screen().cursor_position(), (0, 4));
    assert!(!parser.screen().row_wrapped(0));
    assert_eq!(parser.screen().scrollback_generation(), 0);
    parser.screen_mut().set_size(2, 1);
    parser.screen_mut().set_size(2, 5);
    parser.process(b"X");
    assert_eq!(parser.screen().contents(), "abcdX");
    parser.process(b"\x1b[?7hYZ");
    assert_eq!(parser.screen().contents(), "abcdYZ");
    assert!(parser.screen().row_wrapped(0));
}

#[test]
fn no_wrap_handles_wide_right_margin_and_one_column_combining_output() {
    let mut parser = Parser::new(2, 3, 100);
    parser.process("\x1b[?7lab界".as_bytes());
    assert_eq!(parser.screen().contents(), "ab界");
    assert_eq!(parser.screen().cell(0, 2).unwrap().natural_width(), 2);
    assert!(!parser.screen().cell(0, 2).unwrap().is_wide());
    assert!(!parser.screen().row_wrapped(0));
    parser.process(b"X");
    assert_eq!(parser.screen().contents(), "abX");
    assert_eq!(parser.screen().scrollback_generation(), 0);

    let mut narrow = Parser::new(1, 1, 100);
    narrow.process("\x1b[?7lA界e\u{301}".as_bytes());
    assert_eq!(narrow.screen().contents(), "e\u{301}");
    assert_eq!(narrow.screen().cursor_position(), (0, 0));
    assert_eq!(narrow.screen().scrollback_generation(), 0);
}

#[test]
fn autowrap_mode_survives_snapshot_save_restore_and_resets_on_ris() {
    let mut parser = Parser::new(2, 5, 100);
    parser.process(b"\x1b[?7labcdef\x1b7\x1b[?7h\x1b8");
    assert!(!parser.screen().autowrap());
    let mut replay = Parser::new(2, 5, 100);
    replay.process(&parser.screen().state_formatted());
    assert!(!replay.screen().autowrap());
    assert_eq!(replay.screen().contents(), "abcdf");
    replay.process(b"Z");
    assert_eq!(replay.screen().contents(), "abcdZ");
    parser.process(b"\x1bc");
    assert!(parser.screen().autowrap());
}
