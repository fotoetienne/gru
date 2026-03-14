use crate::agent::AgentEvent;
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

/// Maximum characters to display for buffered text chunks.
/// Since text is now buffered for up to 2 seconds, chunks can be larger.
/// 200 characters allows ~3-4 lines of text to display coherently.
const MAX_DISPLAY_CHARS: usize = 200;

/// Configuration for the progress display
pub struct ProgressConfig {
    pub minion_id: String,
    pub issue: String,
    pub quiet: bool,
}

/// Progress display manager for Minion work.
///
/// Consumes normalized `AgentEvent` values to drive the spinner, status bar,
/// and scrolling event log — regardless of which agent backend produced them.
pub struct ProgressDisplay {
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
    pub fn new(config: ProgressConfig) -> Self {
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

    /// Format a timestamp string for display.
    ///
    /// If `ts` is a valid RFC 3339 timestamp, returns the local `HH:MM:SS`.
    /// For legacy events without a timestamp, returns `--:--:--`.
    fn format_timestamp(ts: Option<&str>) -> String {
        match ts {
            Some(s) => DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string())
                .unwrap_or_else(|_| "--:--:--".to_string()),
            None => "--:--:--".to_string(),
        }
    }

    /// Process a normalized agent event and update the display.
    ///
    /// When called from live streaming (agent_runner callback), `ts` is `None`
    /// and the current wall-clock time is used. When replaying from
    /// `events.jsonl`, the persisted timestamp is passed in.
    pub fn handle_event(&self, event: &AgentEvent) {
        self.handle_event_with_ts(event, None);
    }

    /// Process a normalized agent event with an explicit timestamp.
    pub fn handle_event_with_ts(&self, event: &AgentEvent, ts: Option<&str>) {
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

        let timestamp = match ts {
            Some(_) => Self::format_timestamp(ts),
            None => Local::now().format("%H:%M:%S").to_string(),
        };

        match event {
            AgentEvent::Started { .. } => {
                // Flush any buffered text from a previous turn
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
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
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
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
                    let truncated = Self::truncate_string(first_line, 60);
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
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
            }
            AgentEvent::MessageComplete { stop_reason, .. } => {
                // Flush any remaining buffered text
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
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
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
                self.update_status("✅ Finished");
            }
            AgentEvent::Error { message } => {
                // Flush any buffered text before showing error
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
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

    /// Truncate a string to a maximum number of characters (not bytes)
    fn truncate_string(s: &str, max_chars: usize) -> String {
        // Collect up to max_chars + 1 characters to determine if truncation is needed
        let chars: Vec<char> = s.chars().take(max_chars + 1).collect();
        if chars.len() > max_chars {
            // String is too long, truncate it
            format!("{}...", chars[..max_chars].iter().collect::<String>())
        } else {
            // String is max_chars or shorter, return as-is
            s.to_string()
        }
    }

    /// Finish the progress display and show a final message
    pub fn finish_with_message(&self, message: &str) {
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
        // Just verify that the display was created successfully
        // Events are now printed directly to stdout, not stored
        assert_eq!(display.config.minion_id, "M001");
    }

    #[test]
    fn test_quiet_mode_suppresses_output() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };

        let display = ProgressDisplay::new(config);

        // In quiet mode, non-error events shouldn't be printed
        display.handle_event(&AgentEvent::Started { usage: None });

        // Verify quiet mode is enabled (output is suppressed in handle_event)
        assert!(display.config.quiet);
    }

    #[test]
    fn test_format_timestamp_valid_rfc3339() {
        let ts = "2025-01-15T14:30:45.123+00:00";
        let formatted = ProgressDisplay::format_timestamp(Some(ts));
        // Should produce a valid HH:MM:SS string (exact value depends on local timezone)
        assert_eq!(formatted.len(), 8);
        assert!(formatted.contains(':'));
    }

    #[test]
    fn test_format_timestamp_none() {
        let formatted = ProgressDisplay::format_timestamp(None);
        assert_eq!(formatted, "--:--:--");
    }

    #[test]
    fn test_format_timestamp_invalid() {
        let formatted = ProgressDisplay::format_timestamp(Some("not-a-timestamp"));
        assert_eq!(formatted, "--:--:--");
    }
}
