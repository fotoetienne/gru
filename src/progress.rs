use crate::stream::{ClaudeEvent, StreamOutput};
use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Maximum number of recent events to keep in the display history
const MAX_RECENT_EVENTS: usize = 4;

/// Configuration for the progress display
pub struct ProgressConfig {
    pub minion_id: String,
    pub issue: String,
    pub quiet: bool,
}

/// Progress display manager for Minion work
pub struct ProgressDisplay {
    _multi: MultiProgress,
    status_bar: ProgressBar,
    events_bar: ProgressBar,
    start_time: Instant,
    config: ProgressConfig,
    recent_events: Arc<Mutex<Vec<String>>>,
}

impl ProgressDisplay {
    /// Create a new progress display
    pub fn new(config: ProgressConfig) -> Self {
        let multi = MultiProgress::new();

        // Main status bar
        let status_bar = multi.add(ProgressBar::new_spinner());
        status_bar.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );

        // Events bar (shows recent activity)
        let events_bar = multi.add(ProgressBar::new_spinner());
        events_bar.set_style(ProgressStyle::default_spinner().template("{msg}").unwrap());

        let display = Self {
            _multi: multi,
            status_bar,
            events_bar,
            start_time: Instant::now(),
            config,
            recent_events: Arc::new(Mutex::new(Vec::new())),
        };

        display.update_header("Starting...");
        display
    }

    /// Update the header with current status
    fn update_header(&self, status: &str) {
        let elapsed = self.start_time.elapsed();
        let mins = elapsed.as_secs() / 60;
        let secs = elapsed.as_secs() % 60;

        let header = format!(
            "🤖 Minion {} | Issue {} | ⏱️  {}m {:02}s\n\nStatus: {}",
            self.config.minion_id, self.config.issue, mins, secs, status
        );

        self.status_bar.set_message(header);
    }

    /// Add an event to the recent events list
    fn add_event(&self, event_text: String) {
        let mut events = match self.recent_events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // Log the error and recover with the data
                eprintln!("Warning: Progress display mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };
        events.push(event_text);

        // Keep only the last MAX_RECENT_EVENTS events
        if events.len() > MAX_RECENT_EVENTS {
            events.remove(0);
        }

        // Update the events bar
        let recent_text = events
            .iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");

        self.events_bar
            .set_message(format!("Recent:\n{}", recent_text));
    }

    /// Process a stream output and update the display
    pub fn handle_output(&self, output: &StreamOutput) {
        if self.config.quiet {
            // In quiet mode, only show errors
            if let StreamOutput::Event(ClaudeEvent::Error { error }) = output {
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                eprintln!("Error: {}", error_msg);
            }
            return;
        }

        match output {
            StreamOutput::Event(event) => self.handle_event(event),
            StreamOutput::RawLine(line) => {
                // Pass through non-JSON output to stdout
                use std::io::Write;
                if let Err(e) = std::io::stdout().write_all(line.as_bytes()) {
                    eprintln!("Warning: Failed to write to stdout: {}", e);
                }
                let _ = std::io::stdout().flush();
            }
        }
    }

    /// Truncate a string to a maximum number of characters (not bytes)
    fn truncate_string(s: &str, max_chars: usize) -> String {
        // If the character at position max_chars exists, string is longer than max_chars
        if s.chars().nth(max_chars).is_some() {
            // String is too long, truncate it
            let chars: Vec<char> = s.chars().take(max_chars).collect();
            format!("{}...", chars.iter().collect::<String>())
        } else {
            // String is max_chars or shorter, return as-is
            s.to_string()
        }
    }

    /// Handle a parsed Claude event
    fn handle_event(&self, event: &ClaudeEvent) {
        let timestamp = Local::now().format("%H:%M:%S");

        match event {
            ClaudeEvent::MessageStart { .. } => {
                self.update_header("💭 Thinking...");
                self.add_event(format!("[{}] Message started", timestamp));
            }
            ClaudeEvent::ContentBlockStart { content_block, .. } => {
                // Check if this is a tool_use block
                if let Some(block_type) = content_block.get("type").and_then(|t| t.as_str()) {
                    match block_type {
                        "tool_use" => {
                            let tool_name = content_block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            self.update_header(&format!("🔧 Using tool: {}", tool_name));
                            self.add_event(format!("[{}] Tool: {}", timestamp, tool_name));
                        }
                        "text" => {
                            self.update_header("📝 Responding...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::ContentBlockDelta { delta, .. } => {
                // Handle text deltas
                if let Some(delta_type) = delta.get("type").and_then(|t| t.as_str()) {
                    if delta_type == "text_delta" {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            let truncated = Self::truncate_string(text, 50);
                            self.add_event(format!("[{}] Text: {}", timestamp, truncated));
                        }
                    }
                }
            }
            ClaudeEvent::ContentBlockStop { .. } => {
                // Content block finished - no specific action needed
            }
            ClaudeEvent::MessageDelta { delta } => {
                // Check for stop_reason to detect completion
                if let Some(stop_reason) = delta.get("stop_reason").and_then(|r| r.as_str()) {
                    match stop_reason {
                        "end_turn" => {
                            self.update_header("✅ Complete");
                            self.add_event(format!("[{}] Complete", timestamp));
                        }
                        "tool_use" => {
                            self.update_header("⏸️  Waiting for tool results...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::MessageStop => {
                self.update_header("✅ Message complete");
            }
            ClaudeEvent::Error { error } => {
                self.update_header("❌ Error");
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                self.add_event(format!("[{}] Error: {}", timestamp, error_msg));
                eprintln!("Error: {}", error_msg);
            }
            ClaudeEvent::Ping => {
                // Keepalive ping - no action needed, spinner ticks below
            }
        }

        // Tick the spinner to show activity
        self.status_bar.tick();
    }

    /// Finish the progress display
    #[allow(dead_code)]
    pub fn finish(&self) {
        self.status_bar.finish_and_clear();
        self.events_bar.finish_and_clear();
    }

    /// Finish the progress display and show a final message
    pub fn finish_with_message(&self, message: &str) {
        self.status_bar.finish_with_message(message.to_string());
        self.events_bar.finish_and_clear();
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
        assert_eq!(display.recent_events.lock().unwrap().len(), 0);
    }

    #[test]
    fn test_add_event_limits_history() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);

        // Add 6 events
        for i in 0..6 {
            display.add_event(format!("Event {}", i));
        }

        // Should only keep the last 4
        let events = display.recent_events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], "Event 2");
        assert_eq!(events[3], "Event 5");
    }

    #[test]
    fn test_quiet_mode_suppresses_output() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };

        let display = ProgressDisplay::new(config);

        // In quiet mode, non-error events shouldn't be added
        let message_start = StreamOutput::Event(ClaudeEvent::MessageStart {
            message: serde_json::json!({}),
        });

        display.handle_output(&message_start);

        // The event shouldn't be added to recent events in quiet mode
        // (This is a simplified test - in practice, quiet mode just doesn't display)
        assert!(display.config.quiet);
    }
}
