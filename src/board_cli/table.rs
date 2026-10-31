/// Space between table columns.
pub const GAP: &str = "  ";

/// Returns how many characters `text` shows.
fn width(text: &str) -> usize {
    text.chars().count()
}

/// Returns how wide each column is, the widest cell in it.
pub fn widths(rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (at, cell) in row.iter().enumerate() {
            match widths.get_mut(at) {
                Some(widest) => *widest = (*widest).max(width(cell)),
                None => widths.push(width(cell)),
            }
        }
    }
    widths
}

/// Pads the rows into aligned columns, one line per row, with no trailing spaces.
pub fn lines(rows: &[Vec<String>]) -> Vec<String> {
    let widths = widths(rows);
    rows.iter()
        .map(|row| {
            let last = row.len().saturating_sub(1);
            let mut line = String::new();
            for (at, cell) in row.iter().enumerate() {
                if at > 0 {
                    line.push_str(GAP);
                }
                line.push_str(cell);
                if at < last {
                    line.push_str(&" ".repeat(widths[at] - width(cell)));
                }
            }
            line.trim_end_matches(' ').to_string()
        })
        .collect()
}
