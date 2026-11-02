use chrono::NaiveDate;
use flate2::Crc;

use super::csv_field;
use crate::models::{ColumnRow, TaskRow};

/// The column titles of an export.
const HEADER: [&str; 7] = [
    "Board", "Level", "Task", "Subtask", "Column", "Due date", "Repeat",
];
/// The most characters an XLSX cell holds.
const XLSX_CELL_MAX: usize = 32767;
/// The title of the one sheet in an XLSX export.
const SHEET_NAME: &str = "Due dates";
/// The ZIP version needed to extract an XLSX export.
const ZIP_VERSION: u16 = 20;
/// The ZIP flag marking entry names as UTF-8.
const UTF8_FLAG: u16 = 0x0800;
/// The DOS date of 1980-01-01, the earliest a ZIP entry holds.
const DOS_DATE_1980: u16 = 33;
/// The XML declaration that opens each part of an XLSX export.
const XML_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
/// The namespace of the relationship attributes in an XLSX export.
const RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// The file format of a due date export.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Format {
    /// An Excel workbook.
    Xlsx,
    /// Comma-separated values.
    Csv,
}

impl Format {
    /// Returns the format a request names.
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "xlsx" => Some(Self::Xlsx),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    /// Returns the file extension.
    pub(super) fn extension(self) -> &'static str {
        match self {
            Self::Xlsx => "xlsx",
            Self::Csv => "csv",
        }
    }

    /// Returns the content type of the file.
    pub(super) fn content_type(self) -> &'static str {
        match self {
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }
}

/// Which due dates an export holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Level {
    /// Task and subtask due dates.
    All,
    /// Task due dates only.
    Task,
    /// Subtask due dates only.
    Subtask,
}

impl Level {
    /// Returns the level a request names.
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "task" => Some(Self::Task),
            "subtask" => Some(Self::Subtask),
            _ => None,
        }
    }

    /// Returns the name of the rows a level adds.
    fn label(self) -> &'static str {
        match self {
            Self::Subtask => "Subtask",
            Self::All | Self::Task => "Task",
        }
    }
}

/// The days an export spans, either end open.
#[derive(Copy, Clone, Debug)]
pub(super) struct Range {
    /// The first day, included.
    pub(super) from: Option<NaiveDate>,
    /// The last day, included.
    pub(super) to: Option<NaiveDate>,
}

impl Range {
    /// Returns whether the RFC 3339 time `due_at` falls on a day in the range.
    fn holds(self, due_at: &str) -> bool {
        let Ok(due) = chrono::DateTime::parse_from_rfc3339(due_at) else {
            return false;
        };
        let day = due.date_naive();
        self.from.is_none_or(|from| day >= from) && self.to.is_none_or(|to| day <= to)
    }
}

/// Returns the header row of an export.
pub(super) fn header_row() -> Vec<String> {
    HEADER.iter().map(ToString::to_string).collect()
}

/// Adds to `rows` a row for each due date the `tasks` of the board `board` hold in `range` at
/// `level`.
pub(super) fn due_rows(
    rows: &mut Vec<Vec<String>>,
    board: &str,
    columns: &[ColumnRow],
    tasks: &[TaskRow],
    level: Level,
    range: Range,
) {
    for task in tasks {
        let column = columns
            .iter()
            .find(|column| column.id == task.column_id)
            .map_or("", |column| column.name.as_str());
        if level != Level::Subtask
            && let Some(due) = task.due_at.as_deref().filter(|due| range.holds(due))
        {
            rows.push(vec![
                board.to_string(),
                Level::Task.label().to_string(),
                task.name.clone(),
                String::new(),
                column.to_string(),
                due.to_string(),
                task.due_repeat.clone().unwrap_or_default(),
            ]);
        }
        if level == Level::Task {
            continue;
        }
        for subtask in &task.subtasks {
            if let Some(due) = subtask.due_at.as_deref().filter(|due| range.holds(due)) {
                rows.push(vec![
                    board.to_string(),
                    Level::Subtask.label().to_string(),
                    task.name.clone(),
                    subtask.name.clone(),
                    column.to_string(),
                    due.to_string(),
                    String::new(),
                ]);
            }
        }
    }
}

/// Returns `rows` as CSV with a byte order mark, every field quoted and formulas made text.
pub(super) fn csv_bytes(rows: &[Vec<String>]) -> Vec<u8> {
    let mut csv = String::from('\u{feff}');
    for row in rows {
        let fields: Vec<String> = row.iter().map(|field| csv_field(field)).collect();
        csv.push_str(&fields.join(","));
        csv.push_str("\r\n");
    }
    csv.into_bytes()
}

/// Returns the spreadsheet column letters of the zero-based `index`, such as `A` or `AA`.
fn column_letters(index: usize) -> String {
    let mut letters = String::new();
    let mut n = index + 1;
    while n > 0 {
        letters.insert(0, char::from(b'A' + ((n - 1) % 26) as u8));
        n = (n - 1) / 26;
    }
    letters
}

/// Returns `text` escaped for XML and cut to the cell limit, with control characters dropped.
fn xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (count, c) in text.chars().enumerate() {
        if count >= XLSX_CELL_MAX {
            break;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\u{0}'..='\u{8}'
            | '\u{b}'
            | '\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{fffe}'
            | '\u{ffff}' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Returns `rows` as an Excel workbook of one sheet of text cells.
pub(super) fn xlsx_bytes(rows: &[Vec<String>]) -> Vec<u8> {
    let mut sheet = format!(
        r#"{XML_HEAD}<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#
    );
    for (y, row) in rows.iter().enumerate() {
        sheet.push_str(&format!(r#"<row r="{}">"#, y + 1));
        for (x, cell) in row.iter().enumerate() {
            sheet.push_str(&format!(
                r#"<c r="{}{}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                column_letters(x),
                y + 1,
                xml_text(cell)
            ));
        }
        sheet.push_str("</row>");
    }
    sheet.push_str("</sheetData></worksheet>");
    zip_stored(&[
        (
            "[Content_Types].xml",
            format!(
                r#"{XML_HEAD}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#
            ),
        ),
        (
            "_rels/.rels",
            format!(
                r#"{XML_HEAD}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{RELATIONSHIPS}/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
            ),
        ),
        (
            "xl/workbook.xml",
            format!(
                r#"{XML_HEAD}<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="{RELATIONSHIPS}"><sheets><sheet name="{SHEET_NAME}" sheetId="1" r:id="rId1"/></sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            format!(
                r#"{XML_HEAD}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{RELATIONSHIPS}/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#
            ),
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}

/// Returns a zip archive that stores each `(name, text)` file uncompressed.
fn zip_stored(files: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, text) in files {
        let data = text.as_bytes();
        let mut crc = Crc::new();
        crc.update(data);
        let (offset, size, len) = (out.len() as u32, data.len() as u32, name.len() as u16);
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        out.extend_from_slice(&UTF8_FLAG.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(&DOS_DATE_1980.to_le_bytes());
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        central.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        central.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        central.extend_from_slice(&UTF8_FLAG.to_le_bytes());
        central.extend_from_slice(&[0; 2]);
        central.extend_from_slice(&[0; 2]);
        central.extend_from_slice(&DOS_DATE_1980.to_le_bytes());
        central.extend_from_slice(&crc.sum().to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let start = out.len() as u32;
    let count = files.len() as u16;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_rows() -> Vec<Vec<String>> {
        vec![
            header_row(),
            vec![
                "B <&>".to_string(),
                "Task".to_string(),
                "=cmd".to_string(),
                String::new(),
                "To \"do\"".to_string(),
                "2030-01-10T10:00:00Z".to_string(),
                String::new(),
            ],
        ]
    }

    fn le16(bytes: &[u8], at: usize) -> usize {
        usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
    }

    fn le32(bytes: &[u8], at: usize) -> usize {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    }

    fn unzip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = bytes.len() - 22;
        assert_eq!(le32(bytes, end), 0x0605_4b50, "end record");
        let count = le16(bytes, end + 10);
        let mut at = le32(bytes, end + 16);
        let mut files = Vec::new();
        for _ in 0..count {
            assert_eq!(le32(bytes, at), 0x0201_4b50, "central entry");
            let (crc, size, name_len) = (
                le32(bytes, at + 16),
                le32(bytes, at + 24),
                le16(bytes, at + 28),
            );
            let local = le32(bytes, at + 42);
            let name =
                String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).expect("name");
            assert_eq!(le32(bytes, local), 0x0403_4b50, "local header");
            let data = bytes[local + 30 + name_len..local + 30 + name_len + size].to_vec();
            let mut check = Crc::new();
            check.update(&data);
            assert_eq!(check.sum() as usize, crc, "crc of {name}");
            files.push((name, data));
            at += 46 + name_len;
        }
        files
    }

    #[test]
    fn xlsx_should_be_a_zip_of_the_workbook_parts_with_escaped_cells() {
        let files = unzip(&xlsx_bytes(&text_rows()));
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "[Content_Types].xml",
                "_rels/.rels",
                "xl/workbook.xml",
                "xl/_rels/workbook.xml.rels",
                "xl/worksheets/sheet1.xml"
            ]
        );
        let sheet = String::from_utf8(files[4].1.clone()).expect("utf8");
        assert!(sheet.contains(">B &lt;&amp;&gt;</t>"), "{sheet}");
        assert!(sheet.contains("To &quot;do&quot;"), "{sheet}");
        assert!(sheet.contains(r#"<c r="G2" t="inlineStr">"#), "{sheet}");
        assert!(sheet.contains(">Due date</t>"), "{sheet}");
    }

    #[test]
    fn xlsx_should_cap_a_cell_and_drop_control_characters() {
        let rows = vec![vec![format!("a\u{1}{}", "x".repeat(40000))]];
        let files = unzip(&xlsx_bytes(&rows));
        let sheet = String::from_utf8(files[4].1.clone()).expect("utf8");
        assert!(!sheet.contains('\u{1}'));
        assert!(sheet.len() < 33000 + 400, "{}", sheet.len());
    }

    #[test]
    fn csv_should_quote_every_field_and_make_formulas_text() {
        let csv = String::from_utf8(csv_bytes(&text_rows())).expect("utf8");
        assert!(csv.starts_with('\u{feff}'));
        assert!(csv.contains("\"'=cmd\""), "{csv}");
        assert!(csv.contains("\"To \"\"do\"\"\""), "{csv}");
        assert!(csv.contains(
            "\"Board\",\"Level\",\"Task\",\"Subtask\",\"Column\",\"Due date\",\"Repeat\"\r\n"
        ));
    }

    #[test]
    fn range_should_include_both_ends_and_refuse_a_bad_date() {
        let day = |text: &str| NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date");
        let range = Range {
            from: Some(day("2030-01-10")),
            to: Some(day("2030-01-12")),
        };
        assert!(range.holds("2030-01-10T00:00:00Z"));
        assert!(range.holds("2030-01-12T23:59:59Z"));
        assert!(!range.holds("2030-01-09T23:59:59Z"));
        assert!(!range.holds("2030-01-13T00:00:00Z"));
        assert!(!range.holds("not a date"));
        assert!(
            Range {
                from: None,
                to: None
            }
            .holds("2030-01-13T00:00:00Z")
        );
    }
}
