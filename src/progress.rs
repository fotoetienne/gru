use crate::stream::{ClaudeEvent, StreamOutput};
use crate::text_buffer::TextBuffer;
use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Tracks the current tool being used and its accumulated input JSON
#[derive(Default)]
struct ToolInputTracker {
    tool_name: Option<String>,
    input_json: String,
}

impl ToolInputTracker {
    fn start_tool(&mut self, name: String) {
        self.tool_name = Some(name);
        self.input_json.clear();
    }

    fn add_input_chunk(&mut self, chunk: &str) {
        self.input_json.push_str(chunk);
    }

    fn take(&mut self) -> Option<(String, String)> {
        if let Some(name) = self.tool_name.take() {
            let input = std::mem::take(&mut self.input_json);
            Some((name, input))
        } else {
            None
        }
    }
}

/// Progress display manager for Minion work
pub struct ProgressDisplay {
    multi: MultiProgress,
    status_bar: ProgressBar,
    start_time: Instant,
    config: ProgressConfig,
    text_buffer: TextBuffer,
    tool_tracker: Arc<Mutex<ToolInputTracker>>,
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
            tool_tracker: Arc::new(Mutex::new(ToolInputTracker::default())),
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

    /// Process a stream output and update the display
    pub fn handle_output(&self, output: &StreamOutput) {
        if self.config.quiet {
            // In quiet mode, only show errors
            if let StreamOutput::Event(ClaudeEvent::Error { error }) = output {
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                log::error!("{}", error_msg);
            }
            return;
        }

        match output {
            StreamOutput::Event(event) => self.handle_event(event),
            StreamOutput::ToolResult(tool_result) => self.handle_tool_result(tool_result),
            StreamOutput::RawLine(_line) => {
                // Raw lines are no longer printed to avoid clutter
                // Formatted events from handle_event() provide sufficient output
                // Raw lines are still logged to events.jsonl for debugging
            }
        }
    }

    /// Handle a tool result message
    fn handle_tool_result(&self, tool_result: &crate::stream::ToolResult) {
        let timestamp = Local::now().format("%H:%M:%S");

        // Look up the tool name by tool_use_id
        let tool_name = {
            let names = match self.tool_names.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    log::warn!("Tool names mutex poisoned, recovering");
                    poisoned.into_inner()
                }
            };
            names.get(&tool_result.tool_use_id).cloned()
        };

        let formatted = if tool_result.is_error {
            // Format error tool results
            let error_msg = match tool_result.content.as_str() {
                Some(s) => s.to_string(),
                None => format!(
                    "Tool failed with non-string error content: {}",
                    tool_result.content
                ),
            };
            // Show first line of error
            let first_line = error_msg.lines().next().unwrap_or(&error_msg);
            let truncated = Self::truncate_string(first_line, 60);
            if let Some(name) = tool_name {
                format!("[{}] ✗ {} failed: {}", timestamp, name, truncated)
            } else {
                format!("[{}] ✗ Tool failed: {}", timestamp, truncated)
            }
        } else {
            // Format successful tool results
            let size = tool_result
                .content
                .as_str()
                .map(|s| s.len())
                .unwrap_or_else(|| {
                    // For non-string content, estimate size from JSON
                    tool_result.content.to_string().len()
                });
            if let Some(name) = tool_name {
                format!("[{}] ✓ {} completed ({} bytes)", timestamp, name, size)
            } else {
                format!("[{}] ✓ Tool completed ({} bytes)", timestamp, size)
            }
        };

        self.print_event(&formatted);
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

    /// Format tool information for display
    /// Returns a concise, human-readable description of the tool call
    fn format_tool_info(tool_name: &str, input_json: &str) -> String {
        // Try to parse the input JSON
        let input: serde_json::Value = match serde_json::from_str(input_json) {
            Ok(v) => v,
            Err(_) => {
                // If parsing fails, just show the tool name
                return format!("Tool: {}", tool_name);
            }
        };

        match tool_name {
            "Bash" => {
                if let Some(command) = input.get("command").and_then(|c| c.as_str()) {
                    let truncated = Self::truncate_string(command, 60);
                    format!("Run: {}", truncated)
                } else {
                    "Run: bash command".to_string()
                }
            }
            "Read" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    // Show just the filename or last few path components
                    let shortened = Self::shorten_path(file_path);
                    format!("Read: {}", shortened)
                } else {
                    "Read: file".to_string()
                }
            }
            "Write" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    let shortened = Self::shorten_path(file_path);
                    format!("Write: {}", shortened)
                } else {
                    "Write: file".to_string()
                }
            }
            "Edit" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    let shortened = Self::shorten_path(file_path);
                    format!("Edit: {}", shortened)
                } else {
                    "Edit: file".to_string()
                }
            }
            "Grep" => {
                if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                    let truncated = Self::truncate_string(pattern, 40);
                    format!("Search: {}", truncated)
                } else {
                    "Search: pattern".to_string()
                }
            }
            "Glob" => {
                if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                    let truncated = Self::truncate_string(pattern, 40);
                    format!("Find: {}", truncated)
                } else {
                    "Find: files".to_string()
                }
            }
            "Task" => {
                if let Some(description) = input.get("description").and_then(|d| d.as_str()) {
                    let truncated = Self::truncate_string(description, 50);
                    format!("Task: {}", truncated)
                } else {
                    "Task: running agent".to_string()
                }
            }
            "TodoWrite" => "Update todos".to_string(),
            "AskUserQuestion" => "Asking question...".to_string(),
            _ => format!("Tool: {}", tool_name),
        }
    }

    /// Shorten a file path for display
    /// Shows ".../" prefix if path is long, keeping the most relevant parts
    fn shorten_path(path: &str) -> String {
        let path_obj = std::path::Path::new(path);
        let components: Vec<_> = path_obj.components().collect();

        if components.len() <= 3 {
            // Short path, show as-is
            path.to_string()
        } else {
            // Long path, show last 3 components
            let last_parts: Vec<_> = components
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect();
            format!(".../{}", last_parts.join("/"))
        }
    }

    /// Handle a parsed Claude event
    fn handle_event(&self, event: &ClaudeEvent) {
        let timestamp = Local::now().format("%H:%M:%S");

        match event {
            ClaudeEvent::MessageStart { .. } => {
                self.update_status("💭 Thinking...");
                self.print_event(&format!("[{}] Message started", timestamp));
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
                            let tool_id = content_block
                                .get("id")
                                .and_then(|id| id.as_str())
                                .unwrap_or("");

                            self.update_status(&format!("🔧 Using tool: {}", tool_name));

                            // Store tool name for later reference when tool completes
                            if !tool_id.is_empty() {
                                let mut names = match self.tool_names.lock() {
                                    Ok(guard) => guard,
                                    Err(poisoned) => {
                                        log::warn!("Tool names mutex poisoned, recovering");
                                        poisoned.into_inner()
                                    }
                                };
                                names.insert(tool_id.to_string(), tool_name.to_string());
                            }

                            // Start tracking this tool's input
                            let mut tracker = match self.tool_tracker.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => {
                                    log::warn!("Tool tracker mutex poisoned, recovering");
                                    poisoned.into_inner()
                                }
                            };
                            tracker.start_tool(tool_name.to_string());
                        }
                        "text" => {
                            self.update_status("📝 Responding...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::ContentBlockDelta { delta, .. } => {
                // Handle different types of deltas
                if let Some(delta_type) = delta.get("type").and_then(|t| t.as_str()) {
                    match delta_type {
                        "text_delta" => {
                            // Handle text deltas with buffering
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                // Add text to buffer; flush if ready
                                if let Some(flushed_text) = self.text_buffer.add(text) {
                                    let truncated =
                                        Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                                    self.print_event(&truncated);
                                }
                            }
                        }
                        "input_json_delta" => {
                            // Accumulate tool input JSON
                            if let Some(partial_json) =
                                delta.get("partial_json").and_then(|p| p.as_str())
                            {
                                let mut tracker = match self.tool_tracker.lock() {
                                    Ok(guard) => guard,
                                    Err(poisoned) => {
                                        log::warn!("Tool tracker mutex poisoned, recovering");
                                        poisoned.into_inner()
                                    }
                                };
                                tracker.add_input_chunk(partial_json);
                            }
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::ContentBlockStop { .. } => {
                // Content block finished - handle both text and tool blocks

                // First, check if we have a tool to format
                let mut tracker = match self.tool_tracker.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        log::warn!("Tool tracker mutex poisoned, recovering");
                        poisoned.into_inner()
                    }
                };
                let tool_info = tracker.take();

                if let Some((tool_name, input_json)) = tool_info {
                    // Format and display tool information
                    let formatted = Self::format_tool_info(&tool_name, &input_json);
                    self.print_event(&format!("[{}] {}", timestamp, formatted));
                } else {
                    // No tool, check for buffered text
                    if let Some(flushed_text) = self.text_buffer.flush() {
                        let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                        self.print_event(&truncated);
                    }
                }
            }
            ClaudeEvent::MessageDelta { delta } => {
                // Check for stop_reason to detect completion
                if let Some(stop_reason) = delta.get("stop_reason").and_then(|r| r.as_str()) {
                    match stop_reason {
                        "end_turn" => {
                            self.update_status("✅ Complete");
                            self.print_event(&format!("[{}] Complete", timestamp));
                        }
                        "tool_use" => {
                            self.update_status("⏸️  Waiting for tool results...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::MessageStop => {
                // Flush any remaining buffered text before completing
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }
                self.update_status("✅ Message complete");
            }
            ClaudeEvent::Error { error } => {
                // Flush any buffered text before showing error
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.print_event(&truncated);
                }

                self.update_status("❌ Error");
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                self.print_event(&format!("[{}] Error: {}", timestamp, error_msg));
                log::error!("{}", error_msg);
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
        let message_start = StreamOutput::Event(ClaudeEvent::MessageStart {
            message: serde_json::json!({}),
        });

        display.handle_output(&message_start);

        // Verify quiet mode is enabled (output is suppressed in handle_output)
        assert!(display.config.quiet);
    }

    #[test]
    fn test_format_tool_info_bash() {
        let input = r#"{"command":"git status"}"#;
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert_eq!(result, "Run: git status");
    }

    #[test]
    fn test_format_tool_info_bash_long() {
        let input = r#"{"command":"git commit -m 'This is a very long commit message that should be truncated'"}"#;
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert!(result.starts_with("Run: git commit"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_format_tool_info_read() {
        let input = r#"{"file_path":"src/main.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Read", input);
        assert_eq!(result, "Read: src/main.rs");
    }

    #[test]
    fn test_format_tool_info_write() {
        let input = r#"{"file_path":"/Users/test/project/src/lib.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Write", input);
        assert!(result.contains("Write:"));
        assert!(result.contains("lib.rs"));
    }

    #[test]
    fn test_format_tool_info_edit() {
        let input = r#"{"file_path":"src/commands/fix.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Edit", input);
        assert_eq!(result, "Edit: src/commands/fix.rs");
    }

    #[test]
    fn test_format_tool_info_grep() {
        let input = r#"{"pattern":"TODO"}"#;
        let result = ProgressDisplay::format_tool_info("Grep", input);
        assert_eq!(result, "Search: TODO");
    }

    #[test]
    fn test_format_tool_info_glob() {
        let input = r#"{"pattern":"**/*.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Glob", input);
        assert_eq!(result, "Find: **/*.rs");
    }

    #[test]
    fn test_format_tool_info_task() {
        let input = r#"{"description":"Explore codebase"}"#;
        let result = ProgressDisplay::format_tool_info("Task", input);
        assert_eq!(result, "Task: Explore codebase");
    }

    #[test]
    fn test_format_tool_info_todowrite() {
        let input = r#"{"todos":[]}"#;
        let result = ProgressDisplay::format_tool_info("TodoWrite", input);
        assert_eq!(result, "Update todos");
    }

    #[test]
    fn test_format_tool_info_unknown_tool() {
        let input = r#"{"foo":"bar"}"#;
        let result = ProgressDisplay::format_tool_info("UnknownTool", input);
        assert_eq!(result, "Tool: UnknownTool");
    }

    #[test]
    fn test_format_tool_info_invalid_json() {
        let input = "not valid json";
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert_eq!(result, "Tool: Bash");
    }

    #[test]
    fn test_shorten_path_short() {
        let path = "src/main.rs";
        let result = ProgressDisplay::shorten_path(path);
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn test_shorten_path_long() {
        let path = "/Users/test/projects/gru/src/commands/fix.rs";
        let result = ProgressDisplay::shorten_path(path);
        assert_eq!(result, ".../src/commands/fix.rs");
    }

    #[test]
    fn test_tool_input_tracker() {
        let mut tracker = ToolInputTracker::default();

        // Start tracking a tool
        tracker.start_tool("Bash".to_string());
        tracker.add_input_chunk(r#"{"comm"#);
        tracker.add_input_chunk(r#"and":"ls"}"#);

        // Take the accumulated input
        let result = tracker.take();
        assert!(result.is_some());
        let (name, input) = result.unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(input, r#"{"command":"ls"}"#);

        // After taking, tracker should be empty
        assert!(tracker.take().is_none());
    }

    #[test]
    fn test_handle_tool_result_success() {
        use crate::stream::ToolResult;

        let tool_result = ToolResult {
            result_type: "tool_result".to_string(),
            tool_use_id: "toolu_123".to_string(),
            content: serde_json::json!("Command output here"),
            is_error: false,
        };

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        // Tool results are now printed to stdout, we just verify no panic
        display.handle_tool_result(&tool_result);
    }

    #[test]
    fn test_handle_tool_result_error() {
        use crate::stream::ToolResult;

        let tool_result = ToolResult {
            result_type: "tool_result".to_string(),
            tool_use_id: "toolu_456".to_string(),
            content: serde_json::json!("Error: command not found"),
            is_error: true,
        };

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        // Tool results are now printed to stdout, we just verify no panic
        display.handle_tool_result(&tool_result);
    }
}
