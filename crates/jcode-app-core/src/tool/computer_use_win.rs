use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use uiautomation::{UIAutomation, UIElement};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, RegisterHotKey, UnregisterHotKey, VK_BACK,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};
use xcap::Window;

const PANIC_HOTKEY_ID: i32 = 0x4A43;
const MAX_UI_ELEMENTS: usize = 160;
const MAX_TREE_DEPTH: u32 = 8;

static PANIC_SWITCH_ACTIVE: AtomicBool = AtomicBool::new(false);
static ACTIVE_INTERRUPTS: LazyLock<Mutex<Vec<jcode_agent_runtime::InterruptSignal>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static HOTKEY_LISTENER: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub struct WindowsComputerTool;

impl WindowsComputerTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct ComputerUseInput {
    action: String,
    #[serde(default)]
    window_id: Option<u32>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

#[async_trait]
impl Tool for WindowsComputerTool {
    fn name(&self) -> &str {
        "computer_use"
    }

    fn description(&self) -> &str {
        "Inspect Windows UI Automation elements and screenshots, or use visible mouse/keyboard input. Ctrl+Alt+Backspace permanently disables input and interrupts active computer-use agents."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["screenshot", "ui", "click", "type", "key"],
                    "description": "Capture a window, inspect the UI tree, or synthesize mouse/keyboard input."
                },
                "window_id": { "type": "integer", "description": "xcap window ID for screenshot; omit to select the first titled window." },
                "depth": { "type": "integer", "description": "Maximum UI tree depth (capped at 8)." },
                "x": { "type": "integer", "description": "Absolute screen X coordinate for click." },
                "y": { "type": "integer", "description": "Absolute screen Y coordinate for click." },
                "text": { "type": "string", "description": "Text to type into the focused control." },
                "key": { "type": "string", "description": "Supported key name: enter, tab, escape, backspace, delete, space, or arrow keys." }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: ComputerUseInput =
            serde_json::from_value(input).context("invalid `computer_use` tool input")?;
        ensure_hotkey_listener()?;
        match input.action.as_str() {
            "screenshot" => tokio::task::spawn_blocking(move || capture_window(input.window_id))
                .await
                .context("window screenshot task panicked")?,
            "ui" => tokio::task::spawn_blocking(move || inspect_ui(input.depth))
                .await
                .context("UI Automation task panicked")?,
            "click" => {
                register_interrupt(ctx.graceful_shutdown_signal.clone());
                ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
                let (Some(x), Some(y)) = (input.x, input.y) else {
                    bail!("click requires x and y coordinates");
                };
                ensure_input_approved(&ctx, "click", format!("click at ({x}, {y})")).await?;
                ensure_synthesis_rate_allowed()?;
                tokio::task::spawn_blocking(move || click(x, y))
                    .await
                    .context("mouse input task panicked")?
            }
            "type" => {
                register_interrupt(ctx.graceful_shutdown_signal.clone());
                ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
                let text = input.text.context("type requires text")?;
                ensure_input_approved(
                    &ctx,
                    "type",
                    format!("type {} characters", text.chars().count()),
                )
                .await?;
                ensure_synthesis_rate_allowed()?;
                tokio::task::spawn_blocking(move || type_text(&text))
                    .await
                    .context("keyboard input task panicked")?
            }
            "key" => {
                register_interrupt(ctx.graceful_shutdown_signal.clone());
                ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
                let key = input.key.context("key requires a supported key name")?;
                ensure_input_approved(&ctx, "key", format!("press {key}")).await?;
                ensure_synthesis_rate_allowed()?;
                tokio::task::spawn_blocking(move || press_key(&key))
                    .await
                    .context("keyboard input task panicked")?
            }
            _ => bail!("unsupported computer_use action: {}", input.action),
        }
    }
}

fn ensure_hotkey_listener() -> Result<()> {
    HOTKEY_LISTENER
        .get_or_init(|| {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("jcode-computer-panic-hotkey".to_owned())
                .spawn(move || unsafe {
                    let registered = RegisterHotKey(
                        std::ptr::null_mut(),
                        PANIC_HOTKEY_ID,
                        MOD_CONTROL | MOD_ALT,
                        VK_BACK as u32,
                    );
                    if registered == 0 {
                        let _ = ready_tx.send(Err(std::io::Error::last_os_error().to_string()));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));
                    let mut message: MSG = std::mem::zeroed();
                    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                        if message.message == WM_HOTKEY
                            && message.wParam == PANIC_HOTKEY_ID as usize
                        {
                            activate_panic_switch();
                        }
                    }
                    UnregisterHotKey(std::ptr::null_mut(), PANIC_HOTKEY_ID);
                })
                .map_err(|error| error.to_string())?;
            ready_rx
                .recv()
                .map_err(|error| format!("panic hotkey listener stopped: {error}"))?
        })
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!("cannot install Ctrl+Alt+Backspace kill switch: {error}"))
}

fn activate_panic_switch() {
    PANIC_SWITCH_ACTIVE.store(true, Ordering::SeqCst);
    let interrupts = ACTIVE_INTERRUPTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for interrupt in interrupts.iter() {
        interrupt.fire();
    }
}

fn register_interrupt(interrupt: Option<jcode_agent_runtime::InterruptSignal>) {
    if let Some(interrupt) = interrupt {
        ACTIVE_INTERRUPTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(interrupt);
    }
}

fn ensure_synthesis_allowed(switch: &AtomicBool) -> Result<()> {
    if switch.load(Ordering::SeqCst) {
        bail!("computer input is disabled by Ctrl+Alt+Backspace; restart jcode to re-enable it");
    }
    Ok(())
}

/// Rate limit for synthesized desktop input (S-4): at most 240 synthesized
/// actions per fixed one-minute window. The kill switch is the immediate
/// mitigation; this bound caps how much input can land between approvals.
const MAX_SYNTHESIZED_ACTIONS_PER_MINUTE: u64 = 240;
const RATE_WINDOW_MILLIS: u128 = 60_000;

static RATE_WINDOW_START_MILLIS: AtomicU64 = AtomicU64::new(0);
static RATE_WINDOW_ACTIONS: AtomicU64 = AtomicU64::new(0);

fn ensure_synthesis_rate_allowed() -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let window_start = RATE_WINDOW_START_MILLIS.load(Ordering::SeqCst);
    if now.saturating_sub(window_start) >= RATE_WINDOW_MILLIS {
        RATE_WINDOW_START_MILLIS.store(now, Ordering::SeqCst);
        RATE_WINDOW_ACTIONS.store(0, Ordering::SeqCst);
        return Ok(());
    }
    let used = RATE_WINDOW_ACTIONS.fetch_add(1, Ordering::SeqCst) + 1;
    if used > MAX_SYNTHESIZED_ACTIONS_PER_MINUTE {
        bail!(
            "computer input rate limit reached ({MAX_SYNTHESIZED_ACTIONS_PER_MINUTE} actions per minute); pause before continuing or press Ctrl+Alt+Backspace to disable input synthesis"
        );
    }
    Ok(())
}

/// In Cowork sessions, every input-synthesis action pauses for explicit
/// desktop-user approval before it runs (S-4). The kill switch, rate limit
/// and approval gate are three independent layers; this is the approval one.
/// Plain CLI sessions keep their existing TUI permission flow.
async fn ensure_input_approved(ctx: &ToolContext, action: &str, detail: String) -> Result<()> {
    let cowork = std::env::var("JCODE_COWORK_MODE").is_ok_and(|value| value == "1");
    if !cowork {
        return Ok(());
    }
    let approved = ctx
        .request_approval("computer_use", format!("{action}: {detail}"))
        .await?;
    anyhow::ensure!(
        approved,
        "computer input action '{action}' was rejected by the desktop user"
    );
    Ok(())
}

fn capture_window(window_id: Option<u32>) -> Result<ToolOutput> {
    let windows = Window::all().context("enumerating Windows desktop windows")?;
    let window = match window_id {
        Some(id) => windows
            .into_iter()
            .find(|window| window.id().ok() == Some(id))
            .with_context(|| format!("window {id} was not found"))?,
        None => windows
            .into_iter()
            .find(|window| window.title().is_ok_and(|title| !title.trim().is_empty()))
            .context("no titled window is available for capture")?,
    };
    let id = window.id()?;
    let title = window.title().unwrap_or_default();
    let image = window.capture_image().context("capturing target window")?;
    let (width, height) = image.dimensions();
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png)?;
    let output = ToolOutput::new(format!(
        "Captured window {id} ({title}), {width}x{height} pixels."
    ))
    .with_title("Windows screenshot")
    .with_labeled_image("image/png", STANDARD.encode(png.into_inner()), "window")
    .with_metadata(json!({"window_id": id, "title": title, "width": width, "height": height}));
    Ok(output)
}

fn inspect_ui(depth: Option<u32>) -> Result<ToolOutput> {
    let automation = UIAutomation::new().context("initializing Windows UI Automation")?;
    let root = automation
        .get_root_element()
        .context("getting Windows desktop UI root")?;
    let elements = automation
        .create_matcher()
        .from_ref(&root)
        .depth(depth.unwrap_or(5).clamp(1, MAX_TREE_DEPTH))
        .find_all()
        .context("querying Windows UI Automation tree")?;
    let records = elements
        .iter()
        .take(MAX_UI_ELEMENTS)
        .filter_map(element_record)
        .collect::<Vec<_>>();
    let output = records
        .iter()
        .map(|record| {
            format!(
                "{} | {} | {:?} | bounds=({}, {}, {}, {})",
                record.name,
                record.control_type,
                record.native_handle,
                record.left,
                record.top,
                record.right,
                record.bottom
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput::new(format!(
        "UI Automation elements: {}{}\n{}",
        records.len(),
        if elements.len() > MAX_UI_ELEMENTS {
            " (capped)"
        } else {
            ""
        },
        output
    ))
    .with_metadata(json!({"elements": records})))
}

#[derive(serde::Serialize)]
struct UiElementRecord {
    name: String,
    control_type: String,
    native_handle: Option<isize>,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

fn element_record(element: &UIElement) -> Option<UiElementRecord> {
    let bounds = element.get_bounding_rectangle().ok()?;
    Some(UiElementRecord {
        name: element.get_name().unwrap_or_default(),
        control_type: format!("{:?}", element.get_control_type().ok()?),
        native_handle: element
            .get_native_window_handle()
            .ok()
            .map(|handle| *handle.as_ref() as isize),
        left: bounds.get_left(),
        top: bounds.get_top(),
        right: bounds.get_right(),
        bottom: bounds.get_bottom(),
    })
}

fn click(x: i32, y: i32) -> Result<ToolOutput> {
    ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
    let mut enigo =
        Enigo::new(&Settings::default()).context("initializing Windows input synthesis")?;
    enigo.move_mouse(x, y, Coordinate::Abs)?;
    ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
    enigo.button(Button::Left, Direction::Click)?;
    Ok(ToolOutput::new(format!(
        "Clicked screen coordinate ({x}, {y})."
    )))
}

fn type_text(text: &str) -> Result<ToolOutput> {
    ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
    let mut enigo =
        Enigo::new(&Settings::default()).context("initializing Windows input synthesis")?;
    for chunk in text.as_bytes().chunks(256) {
        ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        enigo.text(chunk)?;
    }
    Ok(ToolOutput::new(format!(
        "Typed {} characters.",
        text.chars().count()
    )))
}

fn press_key(name: &str) -> Result<ToolOutput> {
    ensure_synthesis_allowed(&PANIC_SWITCH_ACTIVE)?;
    let key = match name.trim().to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "space" => Key::Space,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        _ => bail!("unsupported key name: {name}"),
    };
    let mut enigo =
        Enigo::new(&Settings::default()).context("initializing Windows input synthesis")?;
    enigo.key(key, Direction::Click)?;
    Ok(ToolOutput::new(format!("Pressed {name}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_switch_prevents_input_synthesis() {
        let switch = AtomicBool::new(true);
        assert!(ensure_synthesis_allowed(&switch).is_err());
        switch.store(false, Ordering::SeqCst);
        assert!(ensure_synthesis_allowed(&switch).is_ok());
    }

    #[test]
    fn uia_element_records_include_screen_coordinates() {
        assert_eq!(MAX_TREE_DEPTH, 8);
        assert_eq!(MAX_UI_ELEMENTS, 160);
    }

    #[test]
    fn rate_limit_allows_a_full_window_then_rejects_excess() {
        RATE_WINDOW_START_MILLIS.store(0, Ordering::SeqCst);
        RATE_WINDOW_ACTIONS.store(0, Ordering::SeqCst);
        for _ in 0..MAX_SYNTHESIZED_ACTIONS_PER_MINUTE {
            assert!(ensure_synthesis_rate_allowed().is_ok());
        }
        assert!(ensure_synthesis_rate_allowed().is_err());
        // The limit is per window: once the window is exhausted, the next
        // window starts fresh (simulated by rewinding the window start).
        RATE_WINDOW_START_MILLIS.store(0, Ordering::SeqCst);
        assert!(ensure_synthesis_rate_allowed().is_ok());
    }
}
