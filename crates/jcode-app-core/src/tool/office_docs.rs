use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rust_xlsxwriter::{Color, Format, Workbook};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const PDF_MIME: &str = "application/pdf";

pub struct OfficeDocsTool;

impl OfficeDocsTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct OfficeDocsInput {
    action: String,
    file_path: String,
    #[serde(default)]
    sheets: Vec<SheetInput>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    sections: Vec<SectionInput>,
    #[serde(default)]
    slides: Vec<SlideInput>,
}

#[derive(Deserialize)]
struct SheetInput {
    name: String,
    headers: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct SectionInput {
    heading: String,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct SlideInput {
    title: String,
    #[serde(default)]
    bullets: Vec<String>,
}

#[async_trait]
impl Tool for OfficeDocsTool {
    fn name(&self) -> &str {
        "office_docs"
    }

    fn description(&self) -> &str {
        "Inspect XLSX, CSV, DOCX, and PDF structure, or create XLSX workbooks, DOCX reports, PPTX decks, and PDF reports."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action", "file_path"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["outline", "create_xlsx", "create_docx", "create_pptx", "create_pdf"],
                    "description": "Inspect a document outline or create an XLSX/DOCX/PPTX/PDF artifact."
                },
                "file_path": { "type": "string", "description": "Workspace-relative output or input path." },
                "sheets": {
                    "type": "array",
                    "description": "Workbook sheets for create_xlsx; each has a name, headers, and string rows.",
                    "items": {
                        "type": "object",
                        "required": ["name", "headers"],
                        "properties": {
                            "name": { "type": "string" },
                            "headers": { "type": "array", "items": { "type": "string" } },
                            "rows": { "type": "array", "items": { "type": "array", "items": { "type": "string" } } }
                        }
                    }
                },
                "title": { "type": "string", "description": "DOCX/PPTX/PDF report title." },
                "sections": {
                    "type": "array",
                    "description": "DOCX/PDF report sections, rendered with heading styles.",
                    "items": {
                        "type": "object",
                        "required": ["heading"],
                        "properties": {
                            "heading": { "type": "string" },
                            "body": { "type": "string" }
                        }
                    }
                },
                "slides": {
                    "type": "array",
                    "description": "PPTX deck slides for create_pptx; each has a title and bullet lines.",
                    "items": {
                        "type": "object",
                        "required": ["title"],
                        "properties": {
                            "title": { "type": "string" },
                            "bullets": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: OfficeDocsInput = serde_json::from_value(input)?;
        // The outline action only reads; every create_* action writes, so it
        // must resolve against the session's writable mount subset.
        let path = if params.action == "outline" {
            ctx.verify_path(Path::new(&params.file_path))?
        } else {
            ctx.verify_write_path(Path::new(&params.file_path))?
        };
        match params.action.as_str() {
            "outline" => {
                let summary = tokio::task::spawn_blocking(move || {
                    crate::context::doc_outline::from_verified_path(&path)
                })
                .await
                .context("document outline task panicked")??;
                Ok(ToolOutput::new(summary).with_title("Document outline"))
            }
            "create_xlsx" => {
                if params.sheets.is_empty() {
                    bail!("create_xlsx requires at least one sheet");
                }
                let sheets = params.sheets;
                create_parent(&path).await?;
                let output_path = path.clone();
                tokio::task::spawn_blocking(move || create_xlsx(&output_path, &sheets))
                    .await
                    .context("XLSX generation task panicked")??;
                notify_artifact(&ctx, path.clone(), XLSX_MIME).await?;
                Ok(
                    ToolOutput::new(format!("Created workbook {}", params.file_path))
                        .with_title(params.file_path),
                )
            }
            "create_docx" => {
                let title = params.title.unwrap_or_else(|| "Report".to_owned());
                let sections = params.sections;
                create_parent(&path).await?;
                let output_path = path.clone();
                tokio::task::spawn_blocking(move || create_docx(&output_path, &title, &sections))
                    .await
                    .context("DOCX generation task panicked")??;
                notify_artifact(&ctx, path.clone(), DOCX_MIME).await?;
                Ok(
                    ToolOutput::new(format!("Created report {}", params.file_path))
                        .with_title(params.file_path),
                )
            }
            "create_pptx" => {
                if params.slides.is_empty() {
                    bail!("create_pptx requires at least one slide");
                }
                let title = params
                    .title
                    .clone()
                    .unwrap_or_else(|| params.slides[0].title.clone());
                let slides = params
                    .slides
                    .iter()
                    .map(|slide| {
                        super::pptx::PptxSlide::new(
                            slide.title.clone(),
                            slide.bullets.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                create_parent(&path).await?;
                let output_path = path.clone();
                tokio::task::spawn_blocking(move || super::pptx::write_deck(&output_path, &title, &slides))
                    .await
                    .context("PPTX generation task panicked")??;
                notify_artifact(&ctx, path.clone(), PPTX_MIME).await?;
                Ok(
                    ToolOutput::new(format!("Created deck {}", params.file_path))
                        .with_title(params.file_path),
                )
            }
            "create_pdf" => {
                let title = params.title.unwrap_or_else(|| "Report".to_owned());
                let sections = params
                    .sections
                    .iter()
                    .map(|section| {
                        jcode_pdf::PdfSection::new(
                            section.heading.clone(),
                            section.body.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                create_parent(&path).await?;
                let output_path = path.clone();
                tokio::task::spawn_blocking(move || jcode_pdf::write_report(&output_path, &title, &sections))
                    .await
                    .context("PDF generation task panicked")??;
                notify_artifact(&ctx, path.clone(), PDF_MIME).await?;
                Ok(
                    ToolOutput::new(format!("Created PDF report {}", params.file_path))
                        .with_title(params.file_path),
                )
            }
            _ => bail!("unsupported office_docs action: {}", params.action),
        }
    }
}

async fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

async fn notify_artifact(ctx: &ToolContext, path: PathBuf, mime_type: &str) -> Result<()> {
    jcode_tool_core::notify_session_artifact_created(&ctx.session_id, path, mime_type)
        .await
        .map_err(anyhow::Error::msg)
}

fn create_xlsx(path: &Path, sheets: &[SheetInput]) -> Result<()> {
    let mut workbook = Workbook::new();
    let header_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xD9EAD3));
    for (sheet_index, sheet) in sheets.iter().enumerate() {
        if sheet.name.trim().is_empty() {
            bail!("worksheet names cannot be empty");
        }
        let worksheet = if sheet_index == 0 {
            workbook.add_worksheet()
        } else {
            workbook.add_worksheet()
        };
        worksheet.set_name(&sheet.name)?;
        for (column, header) in sheet.headers.iter().enumerate() {
            worksheet.write_string_with_format(0, column as u16, header, &header_format)?;
        }
        for (row_index, row) in sheet.rows.iter().enumerate() {
            for (column, value) in row.iter().enumerate() {
                worksheet.write_string((row_index + 1) as u32, column as u16, value)?;
            }
        }
        worksheet.autofit();
    }
    workbook.save(path).context("saving XLSX workbook")
}

fn create_docx(path: &Path, title: &str, sections: &[SectionInput]) -> Result<()> {
    use docx_rs::{Docx, Paragraph, Run};

    let mut document = Docx::new().add_paragraph(
        Paragraph::new()
            .style("Title")
            .add_run(Run::new().add_text(title)),
    );
    for section in sections {
        document = document.add_paragraph(
            Paragraph::new()
                .style("Heading1")
                .add_run(Run::new().add_text(&section.heading)),
        );
        for paragraph in section
            .body
            .split("\n\n")
            .filter(|part| !part.trim().is_empty())
        {
            document = document
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text(paragraph.trim())));
        }
    }
    document
        .build()
        .pack(std::fs::File::create(path)?)
        .context("writing DOCX report")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolExecutionMode;
    use jcode_tool_core::register_session_path_validator;
    use serde_json::json;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn context(session_id: String, working_dir: PathBuf) -> ToolContext {
        ToolContext {
            session_id,
            message_id: "office-test-message".into(),
            tool_call_id: "office-test-call".into(),
            working_dir: Some(working_dir),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
            require_validator: false,
        }
    }

    #[tokio::test]
    async fn creates_multisheet_xlsx_and_notifies_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session = format!("office-xlsx-{}", std::process::id());
        let _path_registration = register_session_path_validator(&session, move |candidate| {
            if candidate.starts_with(&root) {
                Ok(candidate.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });
        let notification_count = std::sync::Arc::new(AtomicUsize::new(0));
        let notification_counter = std::sync::Arc::clone(&notification_count);
        let _artifact_registration =
            jcode_tool_core::register_session_artifact_created_notifier(&session, move |_| {
                let count = std::sync::Arc::clone(&notification_counter);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            });

        let context = context(session, temp.path().to_path_buf());
        let output = OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_xlsx",
                    "file_path": "report.xlsx",
                    "sheets": [
                        { "name": "Sales", "headers": ["Item", "Total"], "rows": [["A", "10"]] },
                        { "name": "Costs", "headers": ["Item", "Total"], "rows": [["B", "5"]] }
                    ]
                }),
                context.clone(),
            )
            .await
            .unwrap();

        assert!(output.output.contains("Created workbook"));
        assert_eq!(notification_count.load(Ordering::SeqCst), 1);
        let summary =
            crate::context::doc_outline::from_verified_path(&temp.path().join("report.xlsx"))
                .unwrap();
        assert!(summary.contains("Sheet: Sales"));
        assert!(summary.contains("Sheet: Costs"));

        assert!(
            OfficeDocsTool::new()
                .execute(
                    json!({ "action": "outline", "file_path": "../outside.xlsx" }),
                    context,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn creates_formatted_docx_report() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session = format!("office-docx-{}", std::process::id());
        let _path_registration = register_session_path_validator(&session, move |candidate| {
            if candidate.starts_with(&root) {
                Ok(candidate.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });
        let context = context(session, temp.path().to_path_buf());
        OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_docx",
                    "file_path": "report.docx",
                    "title": "Quarterly Summary",
                    "sections": [{ "heading": "Findings", "body": "Revenue increased." }]
                }),
                context,
            )
            .await
            .unwrap();

        let summary =
            crate::context::doc_outline::from_verified_path(&temp.path().join("report.docx"))
                .unwrap();
        assert!(summary.contains("Heading 1: Findings"));
        assert!(summary.contains("Tables: 0"));
    }

    #[tokio::test]
    async fn creates_pptx_deck_and_notifies_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session = format!("office-pptx-{}", std::process::id());
        let _path_registration = register_session_path_validator(&session, move |candidate| {
            if candidate.starts_with(&root) {
                Ok(candidate.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });
        let notification_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notification_counter = std::sync::Arc::clone(&notification_count);
        let _artifact_registration =
            jcode_tool_core::register_session_artifact_created_notifier(&session, move |artifact| {
                let count = std::sync::Arc::clone(&notification_counter);
                async move {
                    assert_eq!(artifact.mime_type, PPTX_MIME);
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            });

        let context = context(session, temp.path().to_path_buf());
        let output = OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_pptx",
                    "file_path": "review.pptx",
                    "title": "Quarterly Review",
                    "slides": [
                        { "title": "Overview", "bullets": ["Revenue up", "Churn flat"] },
                        { "title": "Risks" }
                    ]
                }),
                context,
            )
            .await
            .unwrap();

        assert!(output.output.contains("Created deck"));
        assert_eq!(notification_count.load(Ordering::SeqCst), 1);
        let file = std::fs::File::open(temp.path().join("review.pptx")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut presentation = String::new();
        archive
            .by_name("ppt/presentation.xml")
            .unwrap()
            .read_to_string(&mut presentation)
            .unwrap();
        assert_eq!(presentation.matches("<p:sldId ").count(), 2);
    }

    #[tokio::test]
    async fn creates_pdf_report_extractable_text() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session = format!("office-pdf-{}", std::process::id());
        let _path_registration = register_session_path_validator(&session, move |candidate| {
            if candidate.starts_with(&root) {
                Ok(candidate.to_path_buf())
            } else {
                Err("outside workspace".to_owned())
            }
        });
        let context = context(session, temp.path().to_path_buf());
        let output = OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_pdf",
                    "file_path": "summary.pdf",
                    "title": "Weekly Summary",
                    "sections": [{ "heading": "Findings", "body": "Costs decreased." }]
                }),
                context,
            )
            .await
            .unwrap();

        assert!(output.output.contains("Created PDF report"));
        let text = jcode_pdf::extract_text(&temp.path().join("summary.pdf")).unwrap();
        assert!(text.contains("Weekly Summary"));
        assert!(text.contains("Costs decreased."));
    }

    #[tokio::test]
    async fn write_actions_are_denied_on_read_only_mounts() {
        // Session models a mixed project: /data is read-only, /workspace is
        // read/write. Reads work on both; office writes only inside /workspace.
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let session = format!("office-readonly-{}", std::process::id());
        let data_for_read = data.clone();
        let workspace_for_read = workspace.clone();
        let _read_registration =
            register_session_path_validator(&session, move |candidate| {
                if candidate.starts_with(&data_for_read)
                    || candidate.starts_with(&workspace_for_read)
                {
                    Ok(candidate.to_path_buf())
                } else {
                    Err("outside workspace".to_owned())
                }
            });
        let workspace_for_write = workspace.clone();
        let _write_registration =
            jcode_tool_core::register_session_write_path_validator(&session, move |candidate| {
                if candidate.starts_with(&workspace_for_write) {
                    Ok(candidate.to_path_buf())
                } else {
                    Err("SECURITY_VIOLATION: mount is read-only".to_owned())
                }
            });

        let context = context(session, temp.path().to_path_buf());
        let read_only_report = OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_docx",
                    "file_path": "data/report.docx",
                    "title": "Blocked",
                    "sections": []
                }),
                context.clone(),
            )
            .await;
        assert!(
            read_only_report.is_err(),
            "write into a read-only mount must fail"
        );

        let allowed_report = OfficeDocsTool::new()
            .execute(
                json!({
                    "action": "create_docx",
                    "file_path": "workspace/report.docx",
                    "title": "Allowed",
                    "sections": []
                }),
                context,
            )
            .await;
        assert!(allowed_report.is_ok(), "write into rw mount must succeed");
    }
}
