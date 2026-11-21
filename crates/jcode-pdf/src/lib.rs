use anyhow::{Result, anyhow};
use std::panic::AssertUnwindSafe;
use std::path::Path;

/// One report section: a bold heading followed by one or more body paragraphs.
#[derive(Debug, Clone)]
pub struct PdfSection {
    pub heading: String,
    pub body: String,
}

impl PdfSection {
    pub fn new(heading: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            body: body.into(),
        }
    }
}

/// Page geometry for [`write_report`]: US Letter with 1-inch margins.
const PAGE_WIDTH: f32 = 612.0;
const PAGE_HEIGHT: f32 = 792.0;
const MARGIN: f32 = 72.0;
const BODY_LINE_HEIGHT: f32 = 15.0;
const HEADING_LINE_HEIGHT: f32 = 26.0;
/// ~5.5pt average glyph width at 11pt Helvetica over a 468pt text column.
const BODY_WRAP_CHARS: usize = 84;

/// Write a paginated text report PDF (Helvetica base-14 fonts, Latin text).
///
/// The writer builds the document with `lopdf` directly, so no additional
/// rendering dependency is pulled in. Base-14 fonts are not embedded, which
/// keeps files small but restricts output to Latin-1 characters; non-Latin
/// text is transliterated into ASCII fallbacks by [`pdf_escape`].
pub fn write_report(path: &Path, title: &str, sections: &[PdfSection]) -> Result<()> {
    use lopdf::{Document, Object, Stream, dictionary};

    if title.trim().is_empty() {
        anyhow::bail!("PDF report title cannot be empty");
    }

    // Layout pass: break the whole report into (font, size, text) lines, then
    // flow them onto pages from the top margin down.
    let mut lines: Vec<(bool, i64, String)> = Vec::new();
    lines.push((true, 20, title.trim().to_owned()));
    lines.push((false, 0, String::new())); // blank spacer after the title
    for section in sections {
        lines.push((true, 14, section.heading.trim().to_owned()));
        for paragraph in section.body.split("\n\n").filter(|p| !p.trim().is_empty()) {
            for line in wrap_text(paragraph.trim(), BODY_WRAP_CHARS) {
                lines.push((false, 11, line));
            }
            lines.push((false, 0, String::new()));
        }
    }

    let first_body_y = PAGE_HEIGHT - MARGIN - 36.0;
    let mut pages: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut y = first_body_y;
    for (bold, size, text) in &lines {
        let step = if *bold && *size > 0 {
            HEADING_LINE_HEIGHT
        } else if *size > 0 {
            BODY_LINE_HEIGHT
        } else {
            BODY_LINE_HEIGHT * 0.6
        };
        if y - step < MARGIN {
            pages.push(std::mem::take(&mut current));
            y = PAGE_HEIGHT - MARGIN;
        }
        if text.is_empty() || *size == 0 {
            y -= step;
            continue;
        }
        let font = if *bold { "F2" } else { "F1" };
        current.extend_from_slice(
            format!(
                "BT 1 0 0 1 {} {} Tm /{} {} Tf ({}) Tj ET\n",
                MARGIN, y, font, size, pdf_escape(text)
            )
            .as_bytes(),
        );
        y -= step;
    }
    if !current.is_empty() || pages.is_empty() {
        pages.push(current);
    }

    let mut doc = Document::with_version("1.4");
    let pages_id = doc.new_object_id();
    let body_font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let bold_font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
    });

    let mut page_ids = Vec::with_capacity(pages.len());
    for content in pages {
        let content_id = doc.add_object(Stream::new(
            dictionary! {
                "Length" => content.len() as i64,
            },
            content,
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Real(PAGE_WIDTH),
                Object::Real(PAGE_HEIGHT),
            ],
            "Contents" => content_id,
            "Resources" => dictionary! {
                "Font" => dictionary! {
                    "F1" => body_font_id,
                    "F2" => bold_font_id,
                },
            },
        });
        page_ids.push(page_id);
    }

    doc.set_object(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Count" => page_ids.len() as i64,
            "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
        },
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.save(path)
        .map(|_| ())
        .map_err(|error| anyhow!("saving PDF report: {error}"))
}

/// Wrap a paragraph into display lines no wider than `max_chars`.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph_line in text.lines() {
        let mut current = String::new();
        for word in paragraph_line.split_whitespace() {
            let would_exceed = !current.is_empty()
                && current.chars().count() + 1 + word.chars().count() > max_chars;
            if would_exceed {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Escape text for a PDF literal string. Base-14 Helvetica renders Latin-1;
/// anything outside is replaced with a '?' so the document stays valid and
/// readable instead of corrupting the stream with unmapped bytes.
fn pdf_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '(' => escaped.push_str("\\("),
            ')' => escaped.push_str("\\)"),
            '\\' => escaped.push_str("\\\\"),
            c if (c as u32) < 256 && !c.is_control() => escaped.push(c),
            _ => escaped.push('?'),
        }
    }
    escaped
}

/// Extract text from a PDF.
///
/// `pdf_extract` panics on some exotic/malformed PDFs (e.g. "unexpected entry
/// in unicode map"). A panic inside a tool task kills the task and surfaces a
/// raw panic to the model, so recover it into a normal error (see #573).
pub fn extract_text(path: &Path) -> Result<String> {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| pdf_extract::extract_text(path)));
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(err)) => Err(err.into()),
        Err(payload) => Err(anyhow!(
            "PDF text extraction failed (parser panic: {})",
            panic_message(&payload)
        )),
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn malformed_pdf_returns_error_instead_of_panicking() {
        let dir = std::env::temp_dir().join("jcode-pdf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("malformed.pdf");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"%PDF-1.7\nnot a real pdf body\n%%EOF\n")
            .unwrap();
        drop(f);

        let err = extract_text(&path).err();
        assert!(err.is_some(), "expected an error for a malformed PDF");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_report_round_trips_through_extraction() {
        let dir = std::env::temp_dir().join("jcode-pdf-test-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("report-{}.pdf", std::process::id()));

        let long_body = "The quarterly review identified measurable gains. ".repeat(12);
        write_report(
            &path,
            "Quarterly Summary",
            &[
                PdfSection::new("Findings", &long_body),
                PdfSection::new("Next Steps", "Approve the budget (with contingencies).\\"),
            ],
        )
        .expect("write_report should succeed");

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "missing PDF magic header");
        let text = extract_text(&path).expect("extraction should handle our own PDF");
        assert!(text.contains("Quarterly Summary"), "title missing");
        assert!(text.contains("Findings"), "heading missing");
        assert!(text.contains("measurable gains"), "body missing");
        // Escaped literal-string characters survive as themselves.
        assert!(text.contains("contingencies"), "escaped parens text missing");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_report_paginates_long_content() {
        let dir = std::env::temp_dir().join("jcode-pdf-test-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("paginated-{}.pdf", std::process::id()));

        // ~130 wrapped lines at 15pt line height, well past one page's 648pt
        // text column.
        let body = "Line content for pagination testing. ".repeat(300);
        write_report(&path, "Long Report", &[PdfSection::new("All Lines", &body)])
            .expect("write_report should succeed");

        let document = lopdf::Document::load(&path).unwrap();
        let page_count = document.get_pages().len();
        assert!(page_count >= 2, "expected multi-page output, got {page_count}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_report_rejects_empty_title() {
        let dir = std::env::temp_dir().join("jcode-pdf-test-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("empty-{}.pdf", std::process::id()));
        assert!(write_report(&path, "   ", &[]).is_err());
    }

    #[test]
    fn escape_replaces_non_latin_characters_with_placeholders() {
        assert_eq!(pdf_escape("a(b)c\\d"), "a\\(b\\)c\\\\d");
        assert_eq!(pdf_escape("café"), "café");
        assert_eq!(pdf_escape("中文"), "??");
    }
}
