use anyhow::{Context, Result, bail};
use calamine::{Reader, open_workbook_auto};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use zip::ZipArchive;

const SAMPLE_ROWS: usize = 3;
const PDF_FIRST_PAGE_LIMIT: usize = 1600;

pub fn from_verified_path(path: &Path) -> Result<String> {
    match path.extension().and_then(|value| value.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("xlsx") | Some("xlsm") | Some("xls") | Some("xlsb") => outline_workbook(path),
        Some("csv") => outline_csv(path),
        Some("docx") => outline_docx(path),
        Some("pdf") => outline_pdf(path),
        _ => bail!("unsupported document outline format"),
    }
}

fn outline_workbook(path: &Path) -> Result<String> {
    let mut workbook = open_workbook_auto(path).context("opening spreadsheet")?;
    let sheet_names = workbook.sheet_names().to_vec();
    let mut output = format!("Workbook: {}\nSheets: {}", path.display(), sheet_names.len());
    for name in sheet_names {
        let range = workbook
            .worksheet_range(&name)
            .with_context(|| format!("reading worksheet {name}"))?;
        let mut rows = range.rows();
        let headers = rows
            .next()
            .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        output.push_str(&format!(
            "\n\nSheet: {name}\nRows: {}\nColumns: {}\nHeaders: {}",
            range.height().saturating_sub(1),
            range.width(),
            headers.join(" | ")
        ));
        for (index, row) in rows.take(SAMPLE_ROWS).enumerate() {
            output.push_str(&format!(
                "\nSample {}: {}",
                index + 1,
                row.iter().map(ToString::to_string).collect::<Vec<_>>().join(" | ")
            ));
        }
    }
    Ok(output)
}

fn outline_csv(path: &Path) -> Result<String> {
    let mut reader = csv::Reader::from_path(path).context("opening CSV")?;
    let headers = reader.headers()?.clone();
    let mut rows = 0usize;
    let mut samples = Vec::new();
    for record in reader.records() {
        let record = record?;
        rows += 1;
        if samples.len() < SAMPLE_ROWS {
            samples.push(record.iter().collect::<Vec<_>>().join(" | "));
        }
    }
    let mut output = format!(
        "CSV: {}\nRows: {rows}\nColumns: {}\nHeaders: {}",
        path.display(),
        headers.len(),
        headers.iter().collect::<Vec<_>>().join(" | ")
    );
    for (index, row) in samples.iter().enumerate() {
        output.push_str(&format!("\nSample {}: {row}", index + 1));
    }
    Ok(output)
}

#[derive(Default)]
struct DocxOutline {
    headings: Vec<(String, String)>,
    tables: Vec<Vec<Vec<String>>>,
    current_table: Option<Vec<Vec<String>>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
    paragraph_style: Option<String>,
    paragraph_text: String,
    in_paragraph: bool,
}

fn outline_docx(path: &Path) -> Result<String> {
    let file = File::open(path).context("opening validated DOCX")?;
    let mut archive = ZipArchive::new(file).context("reading DOCX archive")?;
    let mut document = archive
        .by_name("word/document.xml")
        .context("DOCX has no word/document.xml")?;
    let mut xml = String::new();
    document.read_to_string(&mut xml)?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut outline = DocxOutline::default();
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(element)) => {
                let name = element.local_name();
                if name.as_ref() == b"p" {
                    outline.in_paragraph = true;
                    outline.paragraph_style = None;
                    outline.paragraph_text.clear();
                } else if name.as_ref() == b"tbl" {
                    outline.current_table = Some(Vec::new());
                } else if name.as_ref() == b"tr" && outline.current_table.is_some() {
                    outline.current_row.clear();
                } else if name.as_ref() == b"tc" && outline.current_table.is_some() {
                    outline.in_cell = true;
                    outline.current_cell.clear();
                } else if name.as_ref() == b"pStyle" && outline.in_paragraph {
                    outline.paragraph_style = element
                        .attributes()
                        .flatten()
                        .find(|attribute| attribute.key.local_name().as_ref() == b"val")
                        .and_then(|attribute| attribute.unescape_value().ok())
                        .map(|value| value.into_owned());
                }
            }
            Ok(quick_xml::events::Event::Empty(element))
                if element.local_name().as_ref() == b"pStyle" && outline.in_paragraph =>
            {
                outline.paragraph_style = element
                    .attributes()
                    .flatten()
                    .find(|attribute| attribute.key.local_name().as_ref() == b"val")
                    .and_then(|attribute| attribute.unescape_value().ok())
                    .map(|value| value.into_owned());
            }
            Ok(quick_xml::events::Event::Text(text)) => {
                let text = text.unescape()?.into_owned();
                if outline.in_cell {
                    outline.current_cell.push_str(&text);
                }
                if outline.in_paragraph {
                    outline.paragraph_text.push_str(&text);
                }
            }
            Ok(quick_xml::events::Event::End(element)) => {
                let name = element.local_name();
                if name.as_ref() == b"tc" && outline.in_cell {
                    outline.current_row.push(std::mem::take(&mut outline.current_cell));
                    outline.in_cell = false;
                } else if name.as_ref() == b"tr" && outline.current_table.is_some() {
                    if let Some(table) = outline.current_table.as_mut() {
                        table.push(std::mem::take(&mut outline.current_row));
                    }
                } else if name.as_ref() == b"tbl" {
                    if let Some(table) = outline.current_table.take() {
                        outline.tables.push(table);
                    }
                } else if name.as_ref() == b"p" {
                    if let Some(style) = outline.paragraph_style.take()
                        && let Some(level) = heading_level(&style)
                        && !outline.paragraph_text.trim().is_empty()
                    {
                        outline.headings.push((level, outline.paragraph_text.trim().to_owned()));
                    }
                    outline.in_paragraph = false;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(error).context("parsing DOCX XML"),
            _ => {}
        }
    }

    let mut output = format!("DOCX: {}\nHeadings:", path.display());
    if outline.headings.is_empty() {
        output.push_str(" none");
    }
    for (level, heading) in outline.headings {
        output.push_str(&format!("\n  {level}: {heading}"));
    }
    output.push_str(&format!("\nTables: {}", outline.tables.len()));
    for (index, table) in outline.tables.iter().enumerate() {
        let columns = table.iter().map(Vec::len).max().unwrap_or(0);
        let headers = table.first().map(|row| row.join(" | ")).unwrap_or_default();
        output.push_str(&format!(
            "\n  Table {}: {} rows x {} columns; first row: {}",
            index + 1,
            table.len(),
            columns,
            headers
        ));
    }
    Ok(output)
}

fn heading_level(style: &str) -> Option<String> {
    let suffix = style
        .strip_prefix("Heading")
        .or_else(|| style.strip_prefix("heading"))?;
    matches!(suffix, "1" | "2" | "3").then(|| format!("Heading {suffix}"))
}

fn outline_pdf(path: &Path) -> Result<String> {
    let document = lopdf::Document::load(path).context("opening PDF")?;
    let page_count = document.get_pages().len();
    let first_page = if page_count == 0 {
        String::new()
    } else {
        document.extract_text(&[1]).unwrap_or_default()
    };
    let first_page = first_page.trim();
    let first_page = if first_page.chars().count() > PDF_FIRST_PAGE_LIMIT {
        let end = first_page
            .char_indices()
            .nth(PDF_FIRST_PAGE_LIMIT)
            .map(|(index, _)| index)
            .unwrap_or(first_page.len());
        format!("{} [truncated]", &first_page[..end])
    } else {
        first_page.to_owned()
    };
    let mut headings = Vec::new();
    collect_bookmarks(&document, &document.bookmarks, 0, &mut headings);
    Ok(format!(
        "PDF: {}\nPages: {page_count}\nBookmarks: {}\n{}\nFirst page:\n{}",
        path.display(),
        headings.len(),
        if headings.is_empty() {
            "No bookmarks".to_owned()
        } else {
            headings.join("\n")
        },
        if first_page.is_empty() {
            "No extractable text".to_owned()
        } else {
            first_page
        }
    ))
}

fn collect_bookmarks(document: &lopdf::Document, ids: &[u32], depth: usize, output: &mut Vec<String>) {
    for id in ids {
        if let Some(bookmark) = document.bookmark_table.get(id) {
            output.push(format!("{}- {}", "  ".repeat(depth), bookmark.title));
            collect_bookmarks(document, &bookmark.children, depth + 1, output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docx_rs::{Docx, Paragraph, Run};

    #[test]
    fn docx_outline_reports_heading_hierarchy_and_tables() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outline.docx");
        let document = Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .style("Heading1")
                    .add_run(Run::new().add_text("Summary")),
            );
        document.build().pack(File::create(&path).unwrap()).unwrap();

        let summary = outline_docx(&path).unwrap();
        assert!(summary.contains("Heading 1: Summary"));
        assert!(summary.contains("Tables: 0"));
    }
}