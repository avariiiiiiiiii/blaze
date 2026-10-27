//! Where the code of a file written otherwise is now: a file whose tokens say what they said,
//! laid out again, keeps its text in the database, and with it everything compiled of it,
//! and what it says of where code is is moved to where the code is written now.
//!
//! Places are found in the text the database has, as a line and a column; the text written
//! now has the same tokens at other offsets, and each token's offset there tells the line and
//! column of the place now.

use std::sync::Arc;

use hir_ty::mir::location::column_width;

/// The text of a file the database has and the text written now, and where each token of the
/// one is in the other.
pub struct Respace {
    was: Arc<str>,
    now: Arc<str>,
    was_lines: Box<[u32]>,
    now_lines: Box<[u32]>,
    /// By where each token begins in `was`: where it begins in `now`, and how far into it a
    /// place can be. A token of imports laid out in another order is had at the start of
    /// them, with nothing in it.
    moves: Box<[(u32, u32, u32)]>,
}

fn lines(text: &str) -> Box<[u32]> {
    let ends = text.bytes().enumerate().filter(|&(_, byte)| byte == b'\n');
    std::iter::once(0).chain(ends.map(|(at, _)| at as u32 + 1)).collect()
}

impl Respace {
    /// `moves` are sorted by where each token begins in `was`.
    pub fn new(was: Arc<str>, now: Arc<str>, moves: Vec<(u32, u32, u32)>) -> Respace {
        let (was_lines, now_lines) = (lines(&was), lines(&now));
        Respace { was, now, was_lines, now_lines, moves: moves.into() }
    }

    /// Whether the text written now is the one the database has.
    pub fn moves_nothing(&self) -> bool {
        self.was == self.now
    }

    /// The text written now.
    pub fn now(&self) -> &Arc<str> {
        &self.now
    }

    /// Where the place at `line` and `column` of the database's text is now, each counted from
    /// one as rustc counts them. A place past the text is where it was.
    pub fn place(&self, line: u32, column: u32) -> (u32, u32) {
        let Some(at) = self.offset(line, column) else { return (line, column) };
        let k = self.moves.partition_point(|it| it.0 <= at);
        let Some(&(was, now, len)) = k.checked_sub(1).map(|k| &self.moves[k]) else {
            return (line, column);
        };
        let at = now + (at - was).min(len);
        let line = self.now_lines.partition_point(|&start| start <= at);
        let start = self.now_lines[line - 1];
        let written = self.now.get(start as usize..at as usize).unwrap_or_default();
        (line as u32, written.chars().map(column_width).sum::<usize>() as u32 + 1)
    }

    /// The offset into the database's text of a line and column.
    fn offset(&self, line: u32, column: u32) -> Option<u32> {
        let start = *self.was_lines.get((line as usize).checked_sub(1)?)?;
        let end = self.was_lines.get(line as usize).map_or(self.was.len() as u32, |it| *it);
        let text = self.was.get(start as usize..end as usize)?;
        let mut width = 1;
        for (at, ch) in text.char_indices() {
            if width >= column as usize {
                return Some(start + at as u32);
            }
            width += column_width(ch);
        }
        Some(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_is_where_its_token_is_now() {
        let (was, now) = ("fn f(){ g(a,b) }\n", "fn f() {\n    g(a, b)\n}\n");
        // fn f ( ) { g ( a , b ) }
        let was_at = [0, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 15];
        let now_at = [0, 3, 4, 5, 7, 13, 14, 15, 16, 18, 19, 21];
        let moves = was_at.iter().zip(&now_at).map(|(&a, &b)| (a, b, 1)).collect();
        let respace = Respace::new(Arc::from(was), Arc::from(now), moves);
        // `g` at line 1, column 9 is at line 2, column 5.
        assert_eq!(respace.place(1, 9), (2, 5));
        // `b` and `}`.
        assert_eq!(respace.place(1, 13), (2, 10));
        assert_eq!(respace.place(1, 16), (3, 1));
        assert_eq!(respace.place(1, 1), (1, 1));
        assert_eq!(respace.place(9, 1), (9, 1));
    }
}
