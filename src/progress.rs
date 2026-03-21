use crate::agent::AgentEvent;
use crate::display_utils::truncate_string;
use crate::text_buffer::TextBuffer;
use chrono::{DateTime, Local};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Locks a mutex, recovering from poison if another thread panicked while holding it.
fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("{} mutex poisoned, recovering", name);
            poisoned.into_inner()
        }
    }
}

/// Formats an optional RFC 3339 timestamp for display.
///
/// Returns `HH:MM:SS` in local time for valid timestamps, or `--:--:--`
/// for `None` (legacy events) or unparseable values.
fn format_timestamp(ts: Option<&str>) -> String {
    match ts {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string())
            .unwrap_or_else(|_| "--:--:--".to_string()),
        None => "--:--:--".to_string(),
    }
}

/// Maximum characters to display for buffered text chunks.
/// Since text is now buffered for up to 2 seconds, chunks can be larger.
/// 200 characters allows ~3-4 lines of text to display coherently.
const MAX_DISPLAY_CHARS: usize = 200;

/// Configuration for the progress display
pub(crate) struct ProgressConfig {
    pub(crate) minion_id: String,
    pub(crate) issue: String,
    pub(crate) quiet: bool,
}

/// Progress display manager for Minion work.
///
/// Consumes normalized `AgentEvent` values to drive the spinner, status bar,
/// and scrolling event log — regardless of which agent backend produced them.
pub(crate) struct ProgressDisplay {
    multi: MultiProgress,
    status_bar: ProgressBar,
    start_time: Instant,
    config: ProgressConfig,
    text_buffer: TextBuffer,
    /// Maps tool_use_id to tool_name for displaying tool completion messages
    tool_names: Arc<Mutex<HashMap<String, String>>>,
}

impl ProgressDisplay {
    /// Create a new progress display
    pub(crate) fn new(config: ProgressConfig) -> Self {
        let multi = MultiProgress::new();

        // Status bar at bottom (single line)
        let status_bar = multi.add(ProgressBar::new_spinner());
        status_bar.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );

        let display = Self {
            multi,
            status_bar,
            start_time: Instant::now(),
            config,
            text_buffer: TextBuffer::new(Duration::from_secs(2)),
            tool_names: Arc::new(Mutex::new(HashMap::new())),
        };

        display.update_status("Starting...");
        display
    }

    /// Update the status bar with current status (single line)
    fn update_status(&self, status: &str) {
        let elapsed = self.start_time.elapsed();
        let mins = elapsed.as_secs() / 60;
        let secs = elapsed.as_secs() % 60;

        let status_line = format!(
            "🤖 Minion {} | Issue {} | ⏱️  {}m {:02}s | {}",
            self.config.minion_id, self.config.issue, mins, secs, status
        );

        self.status_bar.set_message(status_line);
    }

    /// Print an event to stdout (scrolls naturally)
    fn print_event(&self, event_text: &str) {
        // Use MultiProgress::println to coordinate with status bar
        // This ensures events scroll properly while status bar stays at bottom
        let _ = self.multi.println(event_text);
    }

    /// Process a normalized agent event and update the display.
    ///
    /// Uses the current wall-clock time for the timestamp. Suitable for
    /// live streaming where events are displayed in real-time.
    pub(crate) fn handle_event(&self, event: &AgentEvent) {
        let ts = Local::now().format("%H:%M:%S").to_string();
        self.handle_event_inner(event, &ts);
    }

    /// Process a normalized agent event with an explicit timestamp.
    ///
    /// When `ts` is a valid RFC 3339 string, it is converted to local
    /// `HH:MM:SS`. When `ts` is `None` (legacy events without a persisted
    /// timestamp) or unparseable, `--:--:--` is displayed.
    pub(crate) fn handle_event_with_ts(&self, event: &AgentEvent, ts: Option<&str>) {
        let timestamp = format_timestamp(ts);
        self.handle_event_inner(event, &timestamp);
    }

    fn handle_event_inner(&self, event: &AgentEvent, timestamp: &str) {
        if self.config.quiet {
            // In quiet mode, only show errors
            if let AgentEvent::Error { message } = event {
                let msg = if message.is_empty() {
                    "Unknown error"
                } else {
                    message
                };
                log::error!("{}", msg);
            }
            return;
        }

        match event {
            AgentEvent::Started { .. } => {
                // Flush any buffered text from a previous turn
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
                self.update_status("💭 Thinking...");
                self.print_event(&format!("[{}] ▶︎", timestamp));
            }
            AgentEvent::Thinking { .. } => {
                self.update_status("💭 Thinking...");
            }
            AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            } => {
                // Flush any buffered text before showing tool
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }

                let display_name = if tool_name.is_empty() {
                    "unknown"
                } else {
                    tool_name
                };
                self.update_status(&format!("🔧 Using tool: {}", display_name));

                // Use the backend-provided summary when available (e.g., "Run: git status"),
                // otherwise fall back to just the tool name.
                let display_text = input_summary.as_deref().unwrap_or(display_name);
                self.print_event(&format!("[{}] {}", timestamp, display_text));

                // Store display name for later reference when tool completes.
                // Use display_name (not tool_name) so empty names show as "unknown"
                // in ToolResult formatting instead of blank.
                if !tool_use_id.is_empty() {
                    let mut names = lock_or_recover(&self.tool_names, "Tool names");
                    names.insert(tool_use_id.clone(), display_name.to_string());
                }
            }
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // Look up the tool name by tool_use_id
                let tool_name = {
                    let names = lock_or_recover(&self.tool_names, "Tool names");
                    names.get(tool_use_id).cloned()
                };

                let formatted = if *is_error {
                    let first_line = content.lines().next().unwrap_or(content);
                    let truncated = truncate_string(first_line, 60);
                    if let Some(name) = tool_name {
                        format!("[{}] ✗ {} failed: {}", timestamp, name, truncated)
                    } else {
                        format!("[{}] ✗ Tool failed: {}", timestamp, truncated)
                    }
                } else {
                    let size = content.len();
                    if let Some(name) = tool_name {
                        format!("[{}] ✓ {} completed ({} bytes)", timestamp, name, size)
                    } else {
                        format!("[{}] ✓ Tool completed ({} bytes)", timestamp, size)
                    }
                };

                self.print_event(&formatted);
            }
            AgentEvent::TextDelta { text } => {
                self.update_status("📝 Responding...");
                // Add text to buffer; flush if ready
                if let Some(flushed_text) = self.text_buffer.add(text) {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
            }
            AgentEvent::MessageComplete { stop_reason, .. } => {
                // Flush any remaining buffered text
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }

                if let Some(reason) = stop_reason {
                    match reason.as_str() {
                        "end_turn" => {
                            self.update_status("✅ Complete");
                            self.print_event(&format!("[{}] Complete", timestamp));
                        }
                        "tool_use" => {
                            self.update_status("⏸️  Waiting for tool results...");
                        }
                        _ => {
                            self.update_status("✅ Message complete");
                        }
                    }
                } else {
                    self.update_status("✅ Message complete");
                }
            }
            AgentEvent::Finished { .. } => {
                // Flush any remaining buffered text
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
                self.update_status("✅ Finished");
            }
            AgentEvent::Error { message } => {
                // Flush any buffered text before showing error
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }

                self.update_status("❌ Error");
                let error_text = if message.is_empty() {
                    "Unknown error".to_string()
                } else {
                    message.clone()
                };
                self.print_event(&format!("[{}] Error: {}", timestamp, error_text));
                log::error!("{}", error_text);
            }
            AgentEvent::Ping => {
                // Keepalive ping - no action needed, spinner ticks below
            }
        }

        // Tick the spinner to show activity
        self.status_bar.tick();
    }

    /// Finish the progress display and show a final message
    pub(crate) fn finish_with_message(&self, message: &str) {
        self.status_bar.finish_with_message(message.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_display_creation() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        assert_eq!(display.config.minion_id, "M001");
        assert_eq!(display.config.issue, "42");
        assert!(!display.config.quiet);
        // tool_names map starts empty
        let names = lock_or_recover(&display.tool_names, "test");
        assert!(names.is_empty());
    }

    #[test]
    fn test_quiet_mode_skips_tool_name_tracking() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };

        let display = ProgressDisplay::new(config);

        // In quiet mode, non-error events are skipped entirely —
        // tool names should NOT be stored since handle_event_inner returns early.
        display.handle_event(&AgentEvent::ToolUse {
            tool_name: "Bash".to_string(),
            tool_use_id: "tu_123".to_string(),
            input_summary: Some("Run: git status".to_string()),
        });

        let names = lock_or_recover(&display.tool_names, "test");
        assert!(
            names.is_empty(),
            "quiet mode should skip ToolUse processing, but tool_names was populated"
        );
    }

    #[test]
    fn test_tool_use_stores_tool_name() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };
        let display = ProgressDisplay::new(config);

        display.handle_event(&AgentEvent::ToolUse {
            tool_name: "Read".to_string(),
            tool_use_id: "tu_abc".to_string(),
            input_summary: None,
        });

        let names = lock_or_recover(&display.tool_names, "test");
        assert_eq!(names.get("tu_abc").map(String::as_str), Some("Read"));
    }

    #[test]
    fn test_tool_use_empty_name_stored_as_unknown() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };
        let display = ProgressDisplay::new(config);

        display.handle_event(&AgentEvent::ToolUse {
            tool_name: "".to_string(),
            tool_use_id: "tu_xyz".to_string(),
            input_summary: None,
        });

        let names = lock_or_recover(&display.tool_names, "test");
        assert_eq!(names.get("tu_xyz").map(String::as_str), Some("unknown"));
    }

    #[test]
    fn test_handle_event_with_ts_valid_rfc3339() {
        let ts = format_timestamp(Some("2025-01-15T14:30:45.123+00:00"));
        // Should produce a valid HH:MM:SS timestamp (not the placeholder)
        assert_ne!(ts, "--:--:--");
        assert!(
            ts.len() == 8 && ts.chars().filter(|c| *c == ':').count() == 2,
            "expected HH:MM:SS format, got: {}",
            ts
        );
    }

    #[test]
    fn test_handle_event_with_ts_none_shows_placeholder() {
        let ts = format_timestamp(None);
        assert_eq!(ts, "--:--:--");
    }

    #[test]
    fn test_handle_event_with_ts_invalid_shows_placeholder() {
        let ts = format_timestamp(Some("not-a-timestamp"));
        assert_eq!(ts, "--:--:--");
    }
}
