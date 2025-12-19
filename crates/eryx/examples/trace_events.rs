//! Example showing trace events with a colorful terminal UI.
//!
//! This example demonstrates what trace events are emitted for different
//! Python code patterns, with a nice visual display showing execution progress.
//!
//! Run with: `cargo run --example trace_events --features embedded-runtime`

#![allow(clippy::expect_used)]

use std::future::Future;
use std::io::{Write, stdout};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, MoveToColumn, MoveUp, Show},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor, Stylize},
    terminal::{Clear, ClearType},
};
use eryx::{
    CallbackError, JsonSchema, Sandbox, TraceEvent, TraceEventKind, TraceHandler, TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Test Callbacks
// =============================================================================

struct SucceedCallback;

impl TypedCallback for SucceedCallback {
    type Args = ();

    fn name(&self) -> &str {
        "succeed"
    }

    fn description(&self) -> &str {
        "Always succeeds"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"status": "ok"})) })
    }
}

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    message: String,
}

struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the message"
    }

    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"echoed": args.message})) })
    }
}

// =============================================================================
// Visual Trace Handler
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineState {
    Pending,
    Current,
    Executed,
}

#[derive(Clone)]
struct VisualTraceHandler {
    script_lines: Arc<Vec<String>>,
    line_states: Arc<Mutex<Vec<LineState>>>,
    event_count: Arc<AtomicU32>,
    events: Arc<Mutex<Vec<(String, Color)>>>,
    delay_ms: u64,
    title: Arc<String>,
    num_lines: usize,
}

impl VisualTraceHandler {
    fn new(script: &str, delay_ms: u64, title: &str) -> Self {
        let lines: Vec<String> = script.lines().map(|s| s.to_string()).collect();
        let line_count = lines.len();
        // Total height: title + blank + script lines + blank + progress + blank + events header + 4 events + bottom
        let num_lines = 1 + 1 + line_count + 1 + 1 + 1 + 1 + 4 + 1;
        Self {
            script_lines: Arc::new(lines),
            line_states: Arc::new(Mutex::new(vec![LineState::Pending; line_count])),
            event_count: Arc::new(AtomicU32::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
            delay_ms,
            title: Arc::new(title.to_string()),
            num_lines,
        }
    }

    fn render(&self) -> std::io::Result<()> {
        let mut stdout = stdout();
        let states = self.line_states.lock().expect("lock line_states");
        let events = self.events.lock().expect("lock events");

        // Count executed lines for progress
        let executed = states.iter().filter(|s| **s == LineState::Executed).count();
        let current = states.iter().filter(|s| **s == LineState::Current).count();
        let total = states.len();
        let progress = if total > 0 {
            ((executed + current) as f64 / total as f64 * 100.0) as u32
        } else {
            0
        };

        // Box dimensions - inner_width is content between │ and │
        let inner_width: usize = 58;
        let code_width: usize = 42; // Width for code display

        // Title bar: ┌─ Title ─────...─┐ must have same width as └─────...─┘
        // Bottom is: └ + (inner_width × ─) + ┘
        // Title is:  ┌ + ─ + " Title " + (padding × ─) + ┐
        // So: 1 + title_text.len() + padding = inner_width
        let title_text = format!(" {} ", self.title);
        let title_padding = inner_width.saturating_sub(1 + title_text.len());
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("┌─"),
            Print(&title_text),
            Print("─".repeat(title_padding)),
            Print("┐\n"),
            ResetColor
        )?;

        // Blank line
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("│"),
            Print(format!("{:inner_width$}", "")),
            Print("│\n"),
            ResetColor
        )?;

        // Script display with line states
        // Format: "  X NN │ code..." where X is marker, NN is line num
        // Prefix "  X NN │ " = 9 chars, leaving (inner_width - 9) for code
        for (idx, line) in self.script_lines.iter().enumerate() {
            let state = states.get(idx).copied().unwrap_or(LineState::Pending);
            let line_num = idx + 1;

            // Truncate long lines to fit
            let display_line = if line.len() > code_width {
                format!("{}...", &line[..code_width - 3])
            } else {
                format!("{:<code_width$}", line)
            };

            execute!(stdout, SetForegroundColor(Color::Cyan), Print("│"))?;

            match state {
                LineState::Pending => {
                    execute!(
                        stdout,
                        SetForegroundColor(Color::DarkGrey),
                        Print(format!("    {:2} │ {}", line_num, display_line)),
                        ResetColor
                    )?;
                }
                LineState::Current => {
                    execute!(
                        stdout,
                        SetForegroundColor(Color::Yellow),
                        Print(format!("  ► {:2} │ {}", line_num, display_line).bold()),
                        ResetColor
                    )?;
                }
                LineState::Executed => {
                    execute!(
                        stdout,
                        SetForegroundColor(Color::Green),
                        Print(format!("  ✓ {:2} │ {}", line_num, display_line)),
                        ResetColor
                    )?;
                }
            }

            // Pad to inner_width and close: prefix is 9, code is code_width
            let used = 9 + code_width;
            let padding = inner_width.saturating_sub(used);
            execute!(
                stdout,
                Print(format!("{:padding$}", "")),
                SetForegroundColor(Color::Cyan),
                Print("│\n"),
                ResetColor
            )?;
        }

        // Blank line
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("│"),
            Print(format!("{:inner_width$}", "")),
            Print("│\n"),
            ResetColor
        )?;

        // Progress bar
        let bar_width = 30;
        let filled = (progress as usize * bar_width) / 100;
        let empty = bar_width - filled;

        let bar_color = if progress < 50 {
            Color::Yellow
        } else if progress < 100 {
            Color::Cyan
        } else {
            Color::Green
        };

        // "  Progress: " = 12, bar = 30, "  NNN%" = 6, total = 48
        let progress_padding = inner_width.saturating_sub(48);
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("│"),
            ResetColor,
            Print("  Progress: "),
            SetForegroundColor(bar_color),
            Print("█".repeat(filled)),
            SetForegroundColor(Color::DarkGrey),
            Print("░".repeat(empty)),
            ResetColor,
            Print(format!("  {:3}%", progress)),
            Print(format!("{:progress_padding$}", "")),
            SetForegroundColor(Color::Cyan),
            Print("│\n"),
            ResetColor
        )?;

        // Blank line
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("│"),
            Print(format!("{:inner_width$}", "")),
            Print("│\n"),
            ResetColor
        )?;

        // Recent events header: "  Recent Events:" = 16
        let header_padding = inner_width.saturating_sub(16);
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("│"),
            ResetColor,
            Print("  "),
            Print("Recent Events:".bold()),
            Print(format!("{:header_padding$}", "")),
            SetForegroundColor(Color::Cyan),
            Print("│\n"),
            ResetColor
        )?;

        // Recent events (last 4): "    " prefix = 4, text up to 50
        let event_text_width = 50;
        let event_padding = inner_width.saturating_sub(4 + event_text_width);
        let recent: Vec<_> = events.iter().rev().take(4).collect();
        for i in 0..4 {
            execute!(
                stdout,
                SetForegroundColor(Color::Cyan),
                Print("│"),
                ResetColor,
                Print("    ")
            )?;

            if let Some((text, color)) = recent.get(3 - i) {
                let display_text = if text.len() > event_text_width {
                    format!("{}...", &text[..event_text_width - 3])
                } else {
                    format!("{:<event_text_width$}", text)
                };
                execute!(
                    stdout,
                    SetForegroundColor(*color),
                    Print(&display_text),
                    ResetColor
                )?;
            } else {
                execute!(stdout, Print(format!("{:event_text_width$}", "")))?;
            }

            execute!(
                stdout,
                Print(format!("{:event_padding$}", "")),
                SetForegroundColor(Color::Cyan),
                Print("│\n"),
                ResetColor
            )?;
        }

        // Bottom border
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("└"),
            Print("─".repeat(inner_width)),
            Print("┘\n"),
            ResetColor
        )?;

        stdout.flush()?;
        Ok(())
    }

    fn mark_all_executed(&self) {
        let mut states = self.line_states.lock().expect("lock line_states");
        for state in states.iter_mut() {
            if *state == LineState::Current || *state == LineState::Pending {
                *state = LineState::Executed;
            }
        }
    }
}

#[async_trait]
impl TraceHandler for VisualTraceHandler {
    async fn on_trace(&self, event: TraceEvent) {
        let count = self.event_count.fetch_add(1, Ordering::SeqCst) + 1;

        // Update line states
        if event.lineno > 0 {
            let mut states = self.line_states.lock().expect("lock line_states");
            let line_idx = (event.lineno - 1) as usize;

            // Mark previous current line as executed
            for state in states.iter_mut() {
                if *state == LineState::Current {
                    *state = LineState::Executed;
                }
            }

            // Mark new current line
            if line_idx < states.len() && matches!(event.event, TraceEventKind::Line) {
                states[line_idx] = LineState::Current;
            }
        }

        // Log the event with color
        let (desc, color) = match &event.event {
            TraceEventKind::Line => (
                format!("[{:2}] LINE {}", count, event.lineno),
                Color::Yellow,
            ),
            TraceEventKind::Call { function } => {
                (format!("[{:2}] CALL {}()", count, function), Color::Green)
            }
            TraceEventKind::Return { function } => {
                (format!("[{:2}] RETURN {}()", count, function), Color::Blue)
            }
            TraceEventKind::Exception { message } => {
                (format!("[{:2}] EXCEPTION: {}", count, message), Color::Red)
            }
            TraceEventKind::CallbackStart { name } => {
                (format!("[{:2}] → CALLBACK {}", count, name), Color::Magenta)
            }
            TraceEventKind::CallbackEnd { name, duration_ms } => (
                format!("[{:2}] ← CALLBACK {} ({}ms)", count, name, duration_ms),
                Color::Cyan,
            ),
        };

        self.events.lock().expect("lock events").push((desc, color));

        // Re-render in place: move cursor up, clear lines, then render
        let mut stdout = stdout();
        let _ = execute!(stdout, MoveUp(self.num_lines as u16));
        // Clear each line before re-rendering to avoid artifacts
        for _ in 0..self.num_lines {
            let _ = execute!(
                stdout,
                MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                Print("\n")
            );
        }
        let _ = execute!(stdout, MoveUp(self.num_lines as u16));
        let _ = self.render();
        let _ = stdout.flush(); // Ensure output is visible immediately

        // Add a delay for visual effect (this is what makes it visible!)
        if self.delay_ms > 0 {
            thread::sleep(Duration::from_millis(self.delay_ms));
        }
    }
}

// =============================================================================
// Example Runner
// =============================================================================

async fn run_visual_example(
    title: &str,
    script: &str,
    callbacks: Vec<Box<dyn eryx::Callback>>,
    delay_ms: u64,
) -> anyhow::Result<String> {
    let handler = VisualTraceHandler::new(script, delay_ms, title);
    let mut stdout = stdout();

    // Initial render
    handler.render()?;

    // Build sandbox with embedded runtime for fast loading
    let sandbox = Sandbox::embedded()
        .with_trace_handler(handler.clone())
        .with_callbacks(callbacks)
        .build()?;
    let result = sandbox.execute(script).await?;

    // Mark all as executed and do final render
    handler.mark_all_executed();

    // Move up, clear, and render final state
    execute!(stdout, MoveUp(handler.num_lines as u16))?;
    for _ in 0..handler.num_lines {
        execute!(
            stdout,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print("\n")
        )?;
    }
    execute!(stdout, MoveUp(handler.num_lines as u16))?;
    handler.render()?;

    Ok(result.stdout)
}

fn print_header(text: &str) -> std::io::Result<()> {
    let mut stdout = stdout();
    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print("══════════════════════════════════════════════════════════════\n"),
        Print(format!("  {}\n", text).bold()),
        Print("══════════════════════════════════════════════════════════════"),
        ResetColor
    )?;
    println!();
    Ok(())
}

fn print_section(text: &str) -> std::io::Result<()> {
    let mut stdout = stdout();
    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Yellow),
        Print(format!("── {} ──", text).bold()),
        ResetColor
    )?;
    println!();
    println!();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut stdout = stdout();

    // Hide cursor during execution
    execute!(stdout, Hide)?;

    // Ensure we show cursor on exit
    let result = run_demo().await;

    execute!(stdout, Show)?;

    result
}

async fn run_demo() -> anyhow::Result<()> {
    let mut stdout = stdout();

    // Clear screen for clean start
    execute!(stdout, Clear(ClearType::All))?;
    execute!(stdout, crossterm::cursor::MoveTo(0, 0))?;

    print_header("Eryx Trace Events - Visual Demo")?;

    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("This demo shows real-time execution tracing with colorful visualization.\n"),
        Print("Watch as each line is highlighted during execution!\n"),
        ResetColor
    )?;

    // Configurable delay (ms per event) - higher = slower, more visible progress
    let delay_ms = 200;

    // Example 1: Simple assignment
    print_section("Example 1: Simple Assignment")?;

    let output = run_visual_example(
        "Simple Assignment",
        "x = 42\ny = x * 2\nz = x + y",
        vec![],
        delay_ms,
    )
    .await?;

    if !output.is_empty() {
        execute!(
            stdout,
            Print("Output: ".bold()),
            SetForegroundColor(Color::White),
            Print(output.trim()),
            ResetColor,
            Print("\n")
        )?;
    }

    thread::sleep(Duration::from_millis(800));

    // Example 2: Loop
    print_section("Example 2: Loop Execution")?;

    let output = run_visual_example(
        "Loop",
        "total = 0\nfor i in range(3):\n    total += i\nprint(total)",
        vec![],
        delay_ms,
    )
    .await?;

    if !output.is_empty() {
        execute!(
            stdout,
            Print("Output: ".bold()),
            SetForegroundColor(Color::White),
            Print(output.trim()),
            ResetColor,
            Print("\n")
        )?;
    }

    thread::sleep(Duration::from_millis(800));

    // Example 3: Function definition and call
    print_section("Example 3: Function Call")?;

    let output = run_visual_example(
        "Function Call",
        "def greet(name):\n    msg = f'Hello, {name}!'\n    return msg\n\nresult = greet('World')\nprint(result)",
        vec![],
        delay_ms,
    )
    .await?;

    if !output.is_empty() {
        execute!(
            stdout,
            Print("Output: ".bold()),
            SetForegroundColor(Color::White),
            Print(output.trim()),
            ResetColor,
            Print("\n")
        )?;
    }

    thread::sleep(Duration::from_millis(800));

    // Example 4: Callback
    print_section("Example 4: Async Callback")?;

    let output = run_visual_example(
        "Async Callback",
        "result = await succeed()\nprint('Done!')",
        vec![Box::new(SucceedCallback)],
        delay_ms,
    )
    .await?;

    if !output.is_empty() {
        execute!(
            stdout,
            Print("Output: ".bold()),
            SetForegroundColor(Color::White),
            Print(output.trim()),
            ResetColor,
            Print("\n")
        )?;
    }

    thread::sleep(Duration::from_millis(800));

    // Example 5: Multiple callbacks
    print_section("Example 5: Multiple Callbacks")?;

    let output = run_visual_example(
        "Multiple Callbacks",
        "a = await echo(message='Hello')\nb = await succeed()\nprint('All done!')",
        vec![Box::new(EchoCallback), Box::new(SucceedCallback)],
        delay_ms,
    )
    .await?;

    if !output.is_empty() {
        execute!(
            stdout,
            Print("Output: ".bold()),
            SetForegroundColor(Color::White),
            Print(output.trim()),
            ResetColor,
            Print("\n")
        )?;
    }

    // Summary section
    print_section("Summary")?;

    execute!(stdout, Print("Key Points for UI Integration:\n".bold()))?;
    println!();

    execute!(
        stdout,
        SetForegroundColor(Color::Green),
        Print("  • "),
        ResetColor,
        SetForegroundColor(Color::Cyan),
        Print("TraceHandler.on_trace()"),
        ResetColor,
        Print(" is called in "),
        Print("real-time".bold()),
        Print(" during execution\n")
    )?;

    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Green),
        Print("  • "),
        ResetColor,
        Print("For line highlighting:\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("      - Filter for "),
        SetForegroundColor(Color::Cyan),
        Print("TraceEventKind::Line"),
        SetForegroundColor(Color::DarkGrey),
        Print(" events\n"),
        Print("      - Ignore events with "),
        SetForegroundColor(Color::Yellow),
        Print("lineno=0"),
        SetForegroundColor(Color::DarkGrey),
        Print(" (module/callback events)\n"),
        Print("      - Use "),
        SetForegroundColor(Color::Cyan),
        Print("event.lineno"),
        SetForegroundColor(Color::DarkGrey),
        Print(" to highlight the current line\n"),
        ResetColor
    )?;

    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Green),
        Print("  • "),
        ResetColor,
        Print("For callback visualization:\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("      - "),
        SetForegroundColor(Color::Magenta),
        Print("CallbackStart"),
        SetForegroundColor(Color::DarkGrey),
        Print("/"),
        SetForegroundColor(Color::Cyan),
        Print("CallbackEnd"),
        SetForegroundColor(Color::DarkGrey),
        Print(" show when callbacks execute\n"),
        Print("      - "),
        SetForegroundColor(Color::Cyan),
        Print("CallbackEnd"),
        SetForegroundColor(Color::DarkGrey),
        Print(" includes "),
        SetForegroundColor(Color::Yellow),
        Print("duration_ms"),
        SetForegroundColor(Color::DarkGrey),
        Print(" for performance tracking\n"),
        ResetColor
    )?;

    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Green),
        Print("  • "),
        ResetColor,
        Print("All trace events are also collected in "),
        SetForegroundColor(Color::Cyan),
        Print("result.trace[]"),
        ResetColor,
        Print("\n")
    )?;

    println!();
    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print("══════════════════════════════════════════════════════════════\n"),
        SetForegroundColor(Color::Green),
        Print("  Demo Complete!".bold()),
        Print("\n"),
        SetForegroundColor(Color::Cyan),
        Print("══════════════════════════════════════════════════════════════"),
        ResetColor
    )?;
    println!();
    println!();

    Ok(())
}
