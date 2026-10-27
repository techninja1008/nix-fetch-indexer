//! Arranging indicators in the field: pick the most detailed level at which
//! every indicator fits, else the most compact one with scrolling.

/// Indicator detail, most detailed first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// One per line: status, name, bar, phase name, progress readout.
    Full,
    /// Status, name and phase name.
    Wide,
    /// Status and name.
    Short,
    /// Status only: `[T 57]`.
    Mini,
}

/// Width of the status cell `[T 57]`.
pub const STATUS_W: u16 = 6;

impl Level {
    pub const ALL: [Level; 4] = [Level::Full, Level::Wide, Level::Short, Level::Mini];

    /// Narrowest cell for the level, including a one-column gap.
    fn min_cell(self) -> u16 {
        match self {
            Level::Full => 60,
            Level::Wide => 40,
            // The status and at least six characters of the name.
            Level::Short => STATUS_W + 8,
            Level::Mini => STATUS_W + 1,
        }
    }

    /// Cell width the level is laid out at when there is room to spare.
    fn natural_cell(self) -> u16 {
        match self {
            Level::Short => 24,
            l => l.min_cell(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub level: Level,
    pub cols: usize,
    /// Width of each cell, including its trailing gap.
    pub cell_w: u16,
    /// Rows on screen.
    pub rows: usize,
    /// Rows needed for every indicator.
    pub total_rows: usize,
}

impl Grid {
    /// The layout for `n` indicators in a `width`×`height` area.
    pub fn choose(n: usize, width: u16, height: u16) -> Grid {
        let rows = height.max(1) as usize;
        let make = |level: Level| {
            let cols = match level {
                Level::Full => 1,
                // Short cells shrink towards their minimum to fit everything
                // before giving up the names.
                Level::Short => n
                    .div_ceil(rows)
                    .max((width / level.natural_cell()) as usize)
                    .min((width / level.min_cell()) as usize)
                    .max(1),
                l => (width / l.min_cell()).max(1) as usize,
            };
            let cell_w = match level {
                Level::Mini => Level::Mini.min_cell(),
                _ => (width / cols as u16).max(1),
            };
            Grid {
                level,
                cols,
                cell_w,
                rows,
                total_rows: n.div_ceil(cols),
            }
        };
        Level::ALL
            .into_iter()
            .filter(|l| *l == Level::Mini || width >= l.min_cell())
            .map(make)
            .find(|g| g.total_rows <= rows)
            .unwrap_or_else(|| make(Level::Mini))
    }

    /// The largest useful scroll offset (in rows).
    pub fn max_scroll(&self) -> usize {
        self.total_rows.saturating_sub(self.rows)
    }

    /// Adjust `scroll` so that indicator `i` is visible.
    pub fn reveal(&self, i: usize, scroll: usize) -> usize {
        let row = i / self.cols;
        let s = if row < scroll {
            row
        } else if row >= scroll + self.rows {
            row + 1 - self.rows
        } else {
            scroll
        };
        s.min(self.max_scroll())
    }

    /// Which of `n` indicators is at `(x, y)`, relative to the field.
    pub fn hit(&self, n: usize, scroll: usize, x: u16, y: u16) -> Option<usize> {
        let col = (x / self.cell_w) as usize;
        if col >= self.cols || y as usize >= self.rows {
            return None;
        }
        let i = (scroll + y as usize) * self.cols + col;
        (i < n).then_some(i)
    }

    /// Indicators on screen, with their `(x, y)` relative to the field.
    pub fn visible(&self, n: usize, scroll: usize) -> impl Iterator<Item = (usize, u16, u16)> + '_ {
        (0..self.rows).flat_map(move |r| {
            (0..self.cols).filter_map(move |c| {
                let i = (scroll + r) * self.cols + c;
                (i < n).then_some((i, c as u16 * self.cell_w, r as u16))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_most_detailed_fit() {
        let g = Grid::choose(10, 120, 20);
        assert_eq!((g.level, g.cols, g.cell_w), (Level::Full, 1, 120));
        let g = Grid::choose(50, 120, 20);
        assert_eq!(
            (g.level, g.cols, g.cell_w, g.total_rows),
            (Level::Wide, 3, 40, 17)
        );
        let g = Grid::choose(90, 120, 20);
        assert_eq!((g.level, g.cols), (Level::Short, 5));
        // Short cells shrink to fit rather than dropping the names.
        let g = Grid::choose(120, 100, 19);
        assert_eq!((g.level, g.cols, g.cell_w), (Level::Short, 7, 14));
        let g = Grid::choose(300, 120, 20);
        assert_eq!((g.level, g.cols, g.cell_w), (Level::Mini, 17, 7));
        // Too many even for the most compact level: scroll.
        let g = Grid::choose(1000, 80, 20);
        assert_eq!((g.level, g.cols, g.total_rows), (Level::Mini, 11, 91));
        assert_eq!(g.max_scroll(), 71);
        // A narrow terminal never uses full lines.
        assert_eq!(Grid::choose(1, 50, 20).level, Level::Wide);
        assert_eq!(Grid::choose(1, 5, 1).level, Level::Mini);
    }

    #[test]
    fn scrolling_and_hits() {
        let g = Grid::choose(1000, 80, 20);
        assert_eq!(g.reveal(0, 5), 0);
        assert_eq!(g.reveal(11 * 25, 0), 6);
        assert_eq!(g.reveal(11 * 25, 10), 10);
        assert_eq!(g.reveal(999, 0), g.max_scroll());
        assert_eq!(g.hit(1000, 0, 0, 0), Some(0));
        assert_eq!(g.hit(1000, 2, 15, 1), Some(3 * 11 + 2));
        assert_eq!(g.hit(1000, 0, 79, 0), None);
        assert_eq!(g.hit(5, 0, 7 * 6, 0), None);
        let v: Vec<_> = Grid::choose(5, 120, 20).visible(5, 0).collect();
        assert_eq!(v[3], (3, 0, 3));
        let g = Grid::choose(50, 120, 20);
        let v: Vec<_> = g.visible(50, 0).take(4).collect();
        assert_eq!(v, vec![(0, 0, 0), (1, 40, 0), (2, 80, 0), (3, 0, 1)]);
    }
}
