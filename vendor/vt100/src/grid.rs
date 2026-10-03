use crate::term::BufWrite as _;

#[derive(Clone, Debug)]
pub struct Grid {
    size: Size,
    pos: Pos,
    saved_pos: Pos,
    rows: Vec<crate::row::Row>,
    scroll_top: u16,
    scroll_bottom: u16,
    origin_mode: bool,
    saved_origin_mode: bool,
    scrollback: std::collections::VecDeque<crate::row::Row>,
    scrollback_len: usize,
    scrollback_offset: usize,
    history_width: u16,
    history_view_rows: u16,
    scrollback_cells: usize,
    scrollback_lines: usize,
    scrollback_generation: u64,
    reflow_pos: Option<(usize, u16)>,
    reflow_saved_pos: Option<(usize, u16)>,
    // Preserve a cursor's natural offset within a wide glyph even while the
    // one-column viewport can only display that glyph's leading cell.
    reflow_pos_within: u16,
    reflow_saved_pos_within: u16,
}

impl Grid {
    pub fn new(size: Size, scrollback_len: usize) -> Self {
        Self {
            size,
            pos: Pos::default(),
            saved_pos: Pos::default(),
            rows: vec![],
            scroll_top: 0,
            scroll_bottom: size.rows - 1,
            origin_mode: false,
            saved_origin_mode: false,
            scrollback: std::collections::VecDeque::new(),
            scrollback_len,
            scrollback_offset: 0,
            history_width: size.cols,
            history_view_rows: size.rows,
            scrollback_cells: 0,
            scrollback_lines: 0,
            scrollback_generation: 0,
            reflow_pos: None,
            reflow_saved_pos: None,
            reflow_pos_within: 0,
            reflow_saved_pos_within: 0,
        }
    }

    pub fn allocate_rows(&mut self) {
        if self.rows.is_empty() {
            self.rows.extend(
                std::iter::repeat_with(|| {
                    crate::row::Row::new(self.size.cols)
                })
                .take(usize::from(self.size.rows)),
            );
        }
    }

    fn new_row(&self) -> crate::row::Row {
        crate::row::Row::new(self.size.cols)
    }

    pub fn clear(&mut self) {
        self.pos = Pos::default();
        self.saved_pos = Pos::default();
        for row in self.drawing_rows_mut() {
            row.clear(crate::attrs::Attrs::default());
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.size.rows - 1;
        self.origin_mode = false;
        self.saved_origin_mode = false;
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn set_size(&mut self, size: Size) {
        if size.cols != self.size.cols {
            for row in &mut self.rows {
                row.wrap(false);
            }
        }

        if self.scroll_bottom == self.size.rows - 1 {
            self.scroll_bottom = size.rows - 1;
        }

        self.size = size;
        for row in &mut self.rows {
            row.resize(size.cols, crate::Cell::new());
        }
        self.rows.resize(usize::from(size.rows), self.new_row());

        if self.scroll_bottom >= size.rows {
            self.scroll_bottom = size.rows - 1;
        }
        if self.scroll_bottom < self.scroll_top {
            self.scroll_top = 0;
        }

        self.row_clamp_top(false);
        self.row_clamp_bottom(false);
        self.col_clamp();

        if self.saved_pos.row > self.size.rows - 1 {
            self.saved_pos.row = self.size.rows - 1;
        }
        if self.saved_pos.col > self.size.cols - 1 {
            self.saved_pos.col = self.size.cols - 1;
        }
    }

    /// Reflow the primary buffer without serializing through the VT parser.
    /// In particular, this retains parser state, the other buffer, and attributes.
    pub fn set_size_reflow(&mut self, size: Size) {
        if size == self.size {
            return;
        }
        if self.rows.is_empty() {
            self.set_size(size);
            return;
        }
        self.history_width = self.history_width.max(size.cols);
        self.history_view_rows = self.history_view_rows.max(size.rows);
        let old_rows = self.size.rows;
        let old_history = self.scrollback.len();
        let cursor = self.reflow_pos.unwrap_or((
            old_history + usize::from(self.pos.row),
            self.pos.col,
        ));
        let saved = self.reflow_saved_pos.unwrap_or((
            old_history + usize::from(self.saved_pos.row),
            self.saved_pos.col,
        ));
        let anchor_withins =
            [self.reflow_pos_within, self.reflow_saved_pos_within];
        let old_scroll_offset = self.scrollback_offset;
        let mut source: Vec<_> = self.scrollback.drain(..).collect();
        source.append(&mut self.rows);
        // Unused rows below both cursor anchors are viewport padding, not output.
        let last_used = source
            .iter()
            .rposition(|row| row.used_len() > 0 || row.wrapped())
            .unwrap_or(0);
        let keep =
            (last_used.max(cursor.0).max(saved.0) + 1).min(source.len());
        source.truncate(keep);

        let mut output = Vec::new();
        let mut logical = Vec::new();
        let mut anchors = [None, None];
        let mut positions = [(0usize, 0u16); 2];
        let mut position_withins = [0u16; 2];
        for (row_index, row) in source.iter().enumerate() {
            let mut len = if row.wrapped() {
                row.reflow_cells().len()
            } else {
                row.used_len()
            };
            for anchor in [cursor, saved] {
                if anchor.0 == row_index {
                    len = len.max(usize::from(anchor.1));
                }
            }
            len = len.min(row.reflow_cells().len());
            for col in 0..=len {
                for (index, anchor) in [cursor, saved].iter().enumerate() {
                    if anchor.0 == row_index && usize::from(anchor.1) == col {
                        // A position in the second half of a wide cell follows
                        // its lead cell when that cell temporarily has width 1.
                        let continuation = row
                            .reflow_cells()
                            .get(col)
                            .is_some_and(crate::Cell::is_wide_continuation);
                        anchors[index] = Some((
                            logical
                                .len()
                                .saturating_sub(usize::from(continuation)),
                            u16::from(continuation)
                                .max(anchor_withins[index]),
                        ));
                    }
                }
                if let Some(cell) =
                    row.reflow_cells().get(col).filter(|_| col < len)
                {
                    if !cell.is_wide_continuation() {
                        logical.push(cell.clone());
                    }
                }
            }
            if !row.wrapped() || row_index + 1 == source.len() {
                Self::reflow_line(
                    &mut output,
                    &logical,
                    &anchors,
                    &mut positions,
                    &mut position_withins,
                    size.cols,
                );
                logical.clear();
                anchors = [None, None];
            }
        }
        let start = output.len().saturating_sub(usize::from(size.rows));
        self.rows = output.split_off(start);
        self.scrollback = output.into();
        self.size = size;
        self.rows.resize_with(usize::from(size.rows), || {
            crate::row::Row::new(size.cols)
        });
        self.scrollback_cells = self
            .scrollback
            .iter()
            .map(crate::row::Row::retained_cells)
            .sum();
        self.scrollback_lines =
            self.scrollback.iter().filter(|row| !row.wrapped()).count();
        // Budget the complete retained buffer, including its live contents,
        // so a resize only moves content across the seam, never spends a new
        // history allowance and never evicts just because that seam moved.
        self.pos.row = positions[0]
            .0
            .saturating_sub(start)
            .min(usize::from(size.rows - 1)) as u16;
        self.saved_pos.row = positions[1]
            .0
            .saturating_sub(start)
            .min(usize::from(size.rows - 1))
            as u16;
        let removed = self.trim_scrollback();
        for pos in &mut positions {
            pos.0 = pos.0.saturating_sub(removed);
        }
        let start = self.scrollback.len();
        let visible = |pos: (usize, u16)| Pos {
            row: pos.0.saturating_sub(start).min(usize::from(size.rows - 1))
                as u16,
            col: pos.1.min(size.cols),
        };
        self.pos = visible(positions[0]);
        self.saved_pos = visible(positions[1]);
        self.reflow_pos = (positions[0].0 < start).then_some(positions[0]);
        self.reflow_saved_pos =
            (positions[1].0 < start).then_some(positions[1]);
        self.reflow_pos_within = position_withins[0];
        self.reflow_saved_pos_within = position_withins[1];
        self.scrollback_offset = old_scroll_offset.min(self.scrollback.len());
        // A full-screen region remains full-screen. A partial region is clamped
        // exactly as with the upstream grid resize.
        if self.scroll_bottom >= size.rows
            || self.scroll_bottom == old_rows - 1
        {
            self.scroll_bottom = size.rows - 1;
        }
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
        }
    }

    fn reflow_line(
        output: &mut Vec<crate::row::Row>,
        cells: &[crate::Cell],
        anchors: &[Option<(usize, u16)>; 2],
        positions: &mut [(usize, u16); 2],
        position_withins: &mut [u16; 2],
        cols: u16,
    ) {
        let mut row = Vec::new();
        for (index, cell) in cells.iter().enumerate() {
            let width = cell.natural_width().min(cols);
            if row.len() + usize::from(width) > usize::from(cols) {
                output.push(crate::row::Row::from_reflow(
                    std::mem::take(&mut row),
                    cols,
                    true,
                ));
            }
            for (which, anchor) in anchors.iter().enumerate() {
                if let Some((at, within)) = anchor {
                    if *at == index {
                        let natural_within =
                            (*within).min(cell.natural_width() - 1);
                        positions[which] = (
                            output.len(),
                            row.len() as u16 + natural_within.min(width - 1),
                        );
                        position_withins[which] = natural_within;
                    }
                }
            }
            let mut cell = cell.clone();
            cell.set_wide(width == 2);
            cell.set_wide_continuation(false);
            row.push(cell);
            if width == 2 {
                let mut continuation = crate::Cell::new();
                continuation.set_wide_continuation(true);
                row.push(continuation);
            }
        }
        for (which, anchor) in anchors.iter().enumerate() {
            if anchor.is_some_and(|(at, _)| at == cells.len()) {
                positions[which] = (output.len(), row.len() as u16);
                position_withins[which] = 0;
            }
        }
        output.push(crate::row::Row::from_reflow(row, cols, false));
    }

    fn trim_scrollback(&mut self) -> usize {
        // Reserve one maximum-sized viewport so moving live contents into
        // history during a shrink does not evict otherwise-retained output.
        let line_limit = if self.scrollback_len == 0 {
            0
        } else {
            self.scrollback_len
                .saturating_add(usize::from(self.history_view_rows))
        };
        let budget =
            line_limit.saturating_mul(usize::from(self.history_width));
        // Most archives are far below both limits. Avoid scanning the entire
        // live grid for every output row in that common case. The factor of
        // two for a one-column viewport accounts for compressed wide glyphs.
        let max_live_cells = self
            .rows
            .len()
            .saturating_mul(usize::from(self.size.cols.max(2)));
        if self.scrollback_lines.saturating_add(self.rows.len()) <= line_limit
            && self.scrollback_cells.saturating_add(max_live_cells) <= budget
        {
            return 0;
        }
        let live_end = self
            .rows
            .iter()
            .rposition(|row| row.used_len() > 0 || row.wrapped())
            .unwrap_or(0)
            .max(usize::from(self.pos.row))
            .max(usize::from(self.saved_pos.row));
        let live = &self.rows[..(live_end + 1).min(self.rows.len())];
        let live_cells: usize =
            live.iter().map(crate::row::Row::retained_cells).sum();
        let live_lines = live.iter().filter(|row| !row.wrapped()).count();
        let mut removed = 0;
        while self.scrollback_lines.saturating_add(live_lines) > line_limit
            || self.scrollback_cells.saturating_add(live_cells) > budget
        {
            let Some(row) = self.scrollback.pop_front() else {
                break;
            };
            self.scrollback_cells -= row.retained_cells();
            self.scrollback_lines -= usize::from(!row.wrapped());
            removed += 1;
        }
        self.scrollback_offset =
            self.scrollback_offset.min(self.scrollback.len());
        removed
    }

    pub fn scrollback_generation(&self) -> u64 {
        self.scrollback_generation
    }
    pub fn scrollback_rows(&self) -> usize {
        self.scrollback.len()
    }

    pub fn cancel_pending_wrap(&mut self) {
        if self.pos.col >= self.size.cols {
            self.pos.col = self.size.cols - 1;
            self.reflow_pos_within = 0;
        }
    }

    pub fn cancel_saved_pending_wrap(&mut self) {
        if self.saved_pos.col >= self.size.cols {
            self.saved_pos.col = self.size.cols - 1;
            self.reflow_saved_pos_within = 0;
        }
    }

    pub fn pos(&self) -> Pos {
        self.pos
    }

    pub fn set_pos(&mut self, mut pos: Pos) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        if self.origin_mode {
            pos.row = pos.row.saturating_add(self.scroll_top);
        }
        self.pos = pos;
        self.row_clamp_top(self.origin_mode);
        self.row_clamp_bottom(self.origin_mode);
        self.col_clamp();
    }

    pub fn save_cursor(&mut self) {
        self.saved_pos = self.pos;
        self.reflow_saved_pos = self.reflow_pos;
        self.reflow_saved_pos_within = self.reflow_pos_within;
        self.saved_origin_mode = self.origin_mode;
    }

    pub fn restore_cursor(&mut self) {
        self.pos = self.saved_pos;
        self.reflow_pos = self.reflow_saved_pos;
        self.reflow_pos_within = self.reflow_saved_pos_within;
        self.origin_mode = self.saved_origin_mode;
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &crate::row::Row> {
        let scrollback_len = self.scrollback.len();
        let rows_len = self.rows.len();
        self.scrollback
            .iter()
            .skip(scrollback_len - self.scrollback_offset)
            // when scrollback_offset > rows_len (e.g. rows = 3,
            // scrollback_len = 10, offset = 9) the skip(10 - 9)
            // will take 9 rows instead of 3. we need to set
            // the upper bound to rows_len (e.g. 3)
            .take(rows_len)
            // same for rows_len - scrollback_offset (e.g. 3 - 9).
            // it'll panic with overflow. we have to saturate the subtraction.
            .chain(
                self.rows
                    .iter()
                    .take(rows_len.saturating_sub(self.scrollback_offset)),
            )
    }

    pub fn drawing_rows(&self) -> impl Iterator<Item = &crate::row::Row> {
        self.rows.iter()
    }

    pub fn drawing_rows_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut crate::row::Row> {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.reflow_saved_pos = None;
        self.reflow_saved_pos_within = 0;
        self.rows.iter_mut()
    }

    pub fn visible_row(&self, row: u16) -> Option<&crate::row::Row> {
        self.visible_rows().nth(usize::from(row))
    }

    pub fn drawing_row(&self, row: u16) -> Option<&crate::row::Row> {
        self.drawing_rows().nth(usize::from(row))
    }

    pub fn drawing_row_mut(
        &mut self,
        row: u16,
    ) -> Option<&mut crate::row::Row> {
        self.drawing_rows_mut().nth(usize::from(row))
    }

    pub fn current_row_mut(&mut self) -> &mut crate::row::Row {
        self.drawing_row_mut(self.pos.row)
            // we assume self.pos.row is always valid
            .unwrap()
    }

    pub fn visible_cell(&self, pos: Pos) -> Option<&crate::Cell> {
        self.visible_row(pos.row).and_then(|r| r.get(pos.col))
    }

    pub fn drawing_cell(&self, pos: Pos) -> Option<&crate::Cell> {
        self.drawing_row(pos.row).and_then(|r| r.get(pos.col))
    }

    pub fn drawing_cell_mut(&mut self, pos: Pos) -> Option<&mut crate::Cell> {
        self.drawing_row_mut(pos.row)
            .and_then(|r| r.get_mut(pos.col))
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback_len
    }

    pub fn scrollback(&self) -> usize {
        self.scrollback_offset
    }

    pub fn set_scrollback(&mut self, rows: usize) {
        self.scrollback_offset = rows.min(self.scrollback.len());
    }

    pub fn write_contents(&self, contents: &mut String) {
        let mut wrapping = false;
        for row in self.visible_rows() {
            row.write_contents(contents, 0, self.size.cols, wrapping);
            if !row.wrapped() {
                contents.push('\n');
            }
            wrapping = row.wrapped();
        }

        while contents.ends_with('\n') {
            contents.truncate(contents.len() - 1);
        }
    }

    pub fn write_contents_formatted(
        &self,
        contents: &mut Vec<u8>,
    ) -> crate::attrs::Attrs {
        crate::term::ClearAttrs.write_buf(contents);
        crate::term::ClearScreen.write_buf(contents);

        let mut prev_attrs = crate::attrs::Attrs::default();
        let mut prev_pos = Pos::default();
        let mut wrapping = false;
        for (i, row) in self.visible_rows().enumerate() {
            // we limit the number of cols to a u16 (see Size), so
            // visible_rows() can never return more rows than will fit
            let i = i.try_into().unwrap();
            let (new_pos, new_attrs) = row.write_contents_formatted(
                contents,
                0,
                self.size.cols,
                i,
                wrapping,
                Some(prev_pos),
                Some(prev_attrs),
            );
            prev_pos = new_pos;
            prev_attrs = new_attrs;
            wrapping = row.wrapped();
        }

        self.write_cursor_position_formatted(
            contents,
            Some(prev_pos),
            Some(prev_attrs),
        );

        prev_attrs
    }

    pub fn write_contents_diff(
        &self,
        contents: &mut Vec<u8>,
        prev: &Self,
        mut prev_attrs: crate::attrs::Attrs,
    ) -> crate::attrs::Attrs {
        let mut prev_pos = prev.pos;
        let mut wrapping = false;
        let mut prev_wrapping = false;
        for (i, (row, prev_row)) in
            self.visible_rows().zip(prev.visible_rows()).enumerate()
        {
            // we limit the number of cols to a u16 (see Size), so
            // visible_rows() can never return more rows than will fit
            let i = i.try_into().unwrap();
            let (new_pos, new_attrs) = row.write_contents_diff(
                contents,
                prev_row,
                0,
                self.size.cols,
                i,
                wrapping,
                prev_wrapping,
                prev_pos,
                prev_attrs,
            );
            prev_pos = new_pos;
            prev_attrs = new_attrs;
            wrapping = row.wrapped();
            prev_wrapping = prev_row.wrapped();
        }

        self.write_cursor_position_formatted(
            contents,
            Some(prev_pos),
            Some(prev_attrs),
        );

        prev_attrs
    }

    pub fn write_cursor_position_formatted(
        &self,
        contents: &mut Vec<u8>,
        prev_pos: Option<Pos>,
        prev_attrs: Option<crate::attrs::Attrs>,
    ) {
        let prev_attrs = prev_attrs.unwrap_or_default();
        // writing a character to the last column of a row doesn't wrap the
        // cursor immediately - it waits until the next character is actually
        // drawn. it is only possible for the cursor to have this kind of
        // position after drawing a character though, so if we end in this
        // position, we need to redraw the character at the end of the row.
        if prev_pos != Some(self.pos) && self.pos.col >= self.size.cols {
            let mut pos = Pos {
                row: self.pos.row,
                col: self.size.cols - 1,
            };
            if self
                .drawing_cell(pos)
                // we assume self.pos.row is always valid, and self.size.cols
                // - 1 is always a valid column
                .unwrap()
                .is_wide_continuation()
            {
                pos.col = self.size.cols - 2;
            }
            let cell =
                // we assume self.pos.row is always valid, and self.size.cols
                // - 2 must be a valid column because self.size.cols - 1 is
                // always valid and we just checked that the cell at
                // self.size.cols - 1 is a wide continuation character, which
                // means that the first half of the wide character must be
                // before it
                self.drawing_cell(pos).unwrap();
            if cell.has_contents() {
                if let Some(prev_pos) = prev_pos {
                    crate::term::MoveFromTo::new(prev_pos, pos)
                        .write_buf(contents);
                } else {
                    crate::term::MoveTo::new(pos).write_buf(contents);
                }
                cell.attrs().write_escape_code_diff(contents, &prev_attrs);
                contents.extend(cell.contents().as_bytes());
                prev_attrs.write_escape_code_diff(contents, cell.attrs());
            } else {
                // if the cell doesn't have contents, we can't have gotten
                // here by drawing a character in the last column. this means
                // that as far as i'm aware, we have to have reached here from
                // a newline when we were already after the end of an earlier
                // row. in the case where we are already after the end of an
                // earlier row, we can just write a few newlines, otherwise we
                // also need to do the same as above to get ourselves to after
                // the end of a row.
                let mut found = false;
                for i in (0..self.pos.row).rev() {
                    pos.row = i;
                    pos.col = self.size.cols - 1;
                    if self
                        .drawing_cell(pos)
                        // i is always less than self.pos.row, which we assume
                        // to be always valid, so it must also be valid.
                        // self.size.cols - 1 is always a valid col.
                        .unwrap()
                        .is_wide_continuation()
                    {
                        pos.col = self.size.cols - 2;
                    }
                    let cell = self
                        .drawing_cell(pos)
                        // i is always less than self.pos.row, which we assume
                        // to be always valid, so it must also be valid.
                        // self.size.cols - 2 is valid because self.size.cols
                        // - 1 is always valid, and col gets set to
                        // self.size.cols - 2 when the cell at self.size.cols
                        // - 1 is a wide continuation character, meaning that
                        // the first half of the wide character must be before
                        // it
                        .unwrap();
                    if cell.has_contents() {
                        if let Some(prev_pos) = prev_pos {
                            if prev_pos.row != i
                                || prev_pos.col < self.size.cols
                            {
                                crate::term::MoveFromTo::new(prev_pos, pos)
                                    .write_buf(contents);
                                cell.attrs().write_escape_code_diff(
                                    contents,
                                    &prev_attrs,
                                );
                                contents.extend(cell.contents().as_bytes());
                                prev_attrs.write_escape_code_diff(
                                    contents,
                                    cell.attrs(),
                                );
                            }
                        } else {
                            crate::term::MoveTo::new(pos).write_buf(contents);
                            cell.attrs().write_escape_code_diff(
                                contents,
                                &prev_attrs,
                            );
                            contents.extend(cell.contents().as_bytes());
                            prev_attrs.write_escape_code_diff(
                                contents,
                                cell.attrs(),
                            );
                        }
                        contents.extend(
                            "\n".repeat(usize::from(self.pos.row - i))
                                .as_bytes(),
                        );
                        found = true;
                        break;
                    }
                }

                // this can happen if you get the cursor off the end of a row,
                // and then do something to clear the end of the current row
                // without moving the cursor (IL, DL, ED, EL, etc). we know
                // there can't be something in the last column because we
                // would have caught that above, so it should be safe to
                // overwrite it.
                if !found {
                    pos = Pos {
                        row: self.pos.row,
                        col: self.size.cols - 1,
                    };
                    if let Some(prev_pos) = prev_pos {
                        crate::term::MoveFromTo::new(prev_pos, pos)
                            .write_buf(contents);
                    } else {
                        crate::term::MoveTo::new(pos).write_buf(contents);
                    }
                    contents.push(b' ');
                    // we know that the cell has no contents, but it still may
                    // have drawing attributes (background color, etc)
                    let end_cell = self
                        .drawing_cell(pos)
                        // we assume self.pos.row is always valid, and
                        // self.size.cols - 1 is always a valid column
                        .unwrap();
                    end_cell
                        .attrs()
                        .write_escape_code_diff(contents, &prev_attrs);
                    crate::term::SaveCursor.write_buf(contents);
                    crate::term::Backspace.write_buf(contents);
                    crate::term::EraseChar::new(1).write_buf(contents);
                    crate::term::RestoreCursor.write_buf(contents);
                    prev_attrs
                        .write_escape_code_diff(contents, end_cell.attrs());
                }
            }
        } else if let Some(prev_pos) = prev_pos {
            crate::term::MoveFromTo::new(prev_pos, self.pos)
                .write_buf(contents);
        } else {
            crate::term::MoveTo::new(self.pos).write_buf(contents);
        }
    }

    pub fn erase_all(&mut self, attrs: crate::attrs::Attrs) {
        for row in self.drawing_rows_mut() {
            row.clear(attrs);
        }
    }

    pub fn erase_all_forward(&mut self, attrs: crate::attrs::Attrs) {
        let pos = self.pos;
        for row in self.drawing_rows_mut().skip(usize::from(pos.row) + 1) {
            row.clear(attrs);
        }

        self.erase_row_forward(attrs);
    }

    pub fn erase_all_backward(&mut self, attrs: crate::attrs::Attrs) {
        let pos = self.pos;
        for row in self.drawing_rows_mut().take(usize::from(pos.row)) {
            row.clear(attrs);
        }

        self.erase_row_backward(attrs);
    }

    pub fn erase_row(&mut self, attrs: crate::attrs::Attrs) {
        self.current_row_mut().clear(attrs);
    }

    pub fn erase_row_forward(&mut self, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let row = self.current_row_mut();
        for col in pos.col..size.cols {
            row.erase(col, attrs);
        }
    }

    pub fn erase_row_backward(&mut self, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let row = self.current_row_mut();
        for col in 0..=pos.col.min(size.cols - 1) {
            row.erase(col, attrs);
        }
    }

    pub fn insert_cells(&mut self, count: u16) {
        let size = self.size;
        let pos = self.pos;
        let wide = pos.col < size.cols
            && self
                .drawing_cell(pos)
                // we assume self.pos.row is always valid, and we know we are
                // not off the end of a row because we just checked pos.col <
                // size.cols
                .unwrap()
                .is_wide_continuation();
        let row = self.current_row_mut();
        for _ in 0..count {
            if wide {
                row.get_mut(pos.col).unwrap().set_wide_continuation(false);
            }
            row.insert(pos.col, crate::Cell::new());
            if wide {
                row.get_mut(pos.col).unwrap().set_wide_continuation(true);
            }
        }
        row.truncate(size.cols);
    }

    pub fn delete_cells(&mut self, count: u16) {
        let size = self.size;
        let pos = self.pos;
        let row = self.current_row_mut();
        for _ in 0..(count.min(size.cols - pos.col)) {
            row.remove(pos.col);
        }
        row.resize(size.cols, crate::Cell::new());
    }

    pub fn erase_cells(&mut self, count: u16, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let row = self.current_row_mut();
        for col in pos.col..((pos.col.saturating_add(count)).min(size.cols)) {
            row.erase(col, attrs);
        }
    }

    pub fn insert_lines(&mut self, count: u16) {
        for _ in 0..count {
            self.rows.remove(usize::from(self.scroll_bottom));
            self.rows.insert(usize::from(self.pos.row), self.new_row());
            // self.scroll_bottom is maintained to always be a valid row
            self.rows[usize::from(self.scroll_bottom)].wrap(false);
        }
    }

    pub fn delete_lines(&mut self, count: u16) {
        for _ in 0..(count.min(self.size.rows - self.pos.row)) {
            self.rows
                .insert(usize::from(self.scroll_bottom) + 1, self.new_row());
            self.rows.remove(usize::from(self.pos.row));
        }
    }

    pub fn scroll_up(&mut self, count: u16) {
        for _ in 0..(count.min(self.size.rows - self.scroll_top)) {
            self.rows
                .insert(usize::from(self.scroll_bottom) + 1, self.new_row());
            let removed = self.rows.remove(usize::from(self.scroll_top));
            if self.scrollback_len > 0 && !self.scroll_region_active() {
                self.scrollback_cells += removed.retained_cells();
                self.scrollback_lines += usize::from(!removed.wrapped());
                self.scrollback.push_back(removed);
                self.scrollback_generation =
                    self.scrollback_generation.wrapping_add(1);
                self.trim_scrollback();
                if self.scrollback_offset > 0 {
                    self.scrollback_offset =
                        self.scrollback.len().min(self.scrollback_offset + 1);
                }
            }
        }
    }

    pub fn scroll_down(&mut self, count: u16) {
        for _ in 0..count {
            self.rows.remove(usize::from(self.scroll_bottom));
            self.rows
                .insert(usize::from(self.scroll_top), self.new_row());
            // self.scroll_bottom is maintained to always be a valid row
            self.rows[usize::from(self.scroll_bottom)].wrap(false);
        }
    }

    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        let bottom = bottom.min(self.size().rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.size().rows - 1;
        }
        self.pos.row = self.scroll_top;
        self.pos.col = 0;
    }

    fn in_scroll_region(&self) -> bool {
        self.pos.row >= self.scroll_top && self.pos.row <= self.scroll_bottom
    }

    fn scroll_region_active(&self) -> bool {
        self.scroll_top != 0 || self.scroll_bottom != self.size.rows - 1
    }

    pub fn set_origin_mode(&mut self, mode: bool) {
        self.origin_mode = mode;
        self.set_pos(Pos { row: 0, col: 0 });
    }

    pub fn row_inc_clamp(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_add(count);
        self.row_clamp_bottom(in_scroll_region);
    }

    pub fn row_inc_scroll(&mut self, count: u16) -> u16 {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_add(count);
        let lines = self.row_clamp_bottom(in_scroll_region);
        if in_scroll_region {
            self.scroll_up(lines);
            lines
        } else {
            0
        }
    }

    pub fn row_dec_clamp(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_sub(count);
        self.row_clamp_top(in_scroll_region);
    }

    pub fn row_dec_scroll(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        let in_scroll_region = self.in_scroll_region();
        // need to account for clamping by both row_clamp_top and by
        // saturating_sub
        let extra_lines = count.saturating_sub(self.pos.row);
        self.pos.row = self.pos.row.saturating_sub(count);
        let lines = self.row_clamp_top(in_scroll_region);
        self.scroll_down(lines + extra_lines);
    }

    pub fn row_set(&mut self, i: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.row = i;
        self.row_clamp();
    }

    pub fn col_inc(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.col = self.pos.col.saturating_add(count);
    }

    pub fn col_inc_clamp(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.col = self.pos.col.saturating_add(count);
        self.col_clamp();
    }

    pub fn col_dec(&mut self, count: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.col = self.pos.col.saturating_sub(count);
    }

    pub fn col_tab(&mut self) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.col -= self.pos.col % 8;
        self.pos.col += 8;
        self.col_clamp();
    }

    pub fn col_set(&mut self, i: u16) {
        self.reflow_pos = None;
        self.reflow_pos_within = 0;
        self.pos.col = i;
        self.col_clamp();
    }

    pub fn col_wrap(&mut self, width: u16, wrap: bool) {
        if self.pos.col > self.size.cols.saturating_sub(width) {
            let previous_row = self.pos.row;
            // Mark the source before scrolling: on a one-row screen it moves
            // directly into history, so subtracting the scroll count underflows.
            self.drawing_row_mut(previous_row).unwrap().wrap(wrap);
            self.pos.col = 0;
            let scrolled = self.row_inc_scroll(1);
            if scrolled == 0 && self.pos.row != previous_row + 1 {
                self.drawing_row_mut(previous_row).unwrap().wrap(false);
            }
        }
    }

    fn row_clamp_top(&mut self, limit_to_scroll_region: bool) -> u16 {
        if limit_to_scroll_region && self.pos.row < self.scroll_top {
            let rows = self.scroll_top - self.pos.row;
            self.pos.row = self.scroll_top;
            rows
        } else {
            0
        }
    }

    fn row_clamp_bottom(&mut self, limit_to_scroll_region: bool) -> u16 {
        let bottom = if limit_to_scroll_region {
            self.scroll_bottom
        } else {
            self.size.rows - 1
        };
        if self.pos.row > bottom {
            let rows = self.pos.row - bottom;
            self.pos.row = bottom;
            rows
        } else {
            0
        }
    }

    fn row_clamp(&mut self) {
        if self.pos.row > self.size.rows - 1 {
            self.pos.row = self.size.rows - 1;
        }
    }

    fn col_clamp(&mut self) {
        if self.pos.col > self.size.cols - 1 {
            self.pos.col = self.size.cols - 1;
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Pos {
    pub row: u16,
    pub col: u16,
}
