use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::process::{ChildStderr, ChildStdout};

/// Represents the different types of events that Claude Code can emit
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
    #[serde(rename = "thinking")]
    Thinking { content: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },

    #[serde(rename = "message")]
    Message { content: String },

    #[serde(rename = "complete")]
    Complete,

    #[serde(rename = "error")]
    Error { message: String },
}

/// Represents the output from parsing a stream line
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutput {
    /// A parsed Claude event
    Event(ClaudeEvent),
    /// A raw line that wasn't a JSON event
    RawLine(String),
}

/// Synchronous stream reader for Claude Code output
pub struct EventStream<R: BufRead> {
    reader: R,
}

impl EventStream<BufReader<ChildStdout>> {
    /// Create a new EventStream from a child process's stdout
    pub fn from_stdout(stdout: ChildStdout) -> Self {
        Self {
            reader: BufReader::new(stdout),
        }
    }
}

impl EventStream<BufReader<ChildStderr>> {
    /// Create a new EventStream from a child process's stderr
    #[allow(dead_code)]
    pub fn from_stderr(stderr: ChildStderr) -> Self {
        Self {
            reader: BufReader::new(stderr),
        }
    }
}

impl<R: BufRead> EventStream<R> {
    /// Create a new EventStream from any BufRead implementation
    #[allow(dead_code)]
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Read the next line and try to parse it as a Claude event
    /// Returns None if end of stream is reached
    pub fn read_line(&mut self) -> Result<Option<StreamOutput>> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line)?;

        if bytes_read == 0 {
            return Ok(None); // EOF
        }

        let trimmed = line.trim();

        // Skip empty lines
        if trimmed.is_empty() {
            return Ok(Some(StreamOutput::RawLine(line)));
        }

        // Try to parse as JSON event
        match serde_json::from_str::<ClaudeEvent>(trimmed) {
            Ok(event) => Ok(Some(StreamOutput::Event(event))),
            Err(_) => Ok(Some(StreamOutput::RawLine(line))),
        }
    }

    /// Read all lines from the stream
    #[allow(dead_code)]
    pub fn read_all(&mut self) -> Result<Vec<StreamOutput>> {
        let mut outputs = Vec::new();
        while let Some(output) = self.read_line()? {
            outputs.push(output);
        }
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_thinking_event() {
        let json = r#"{"type":"thinking","content":"Analyzing the codebase..."}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Thinking { content } => {
                assert_eq!(content, "Analyzing the codebase...");
            }
            _ => panic!("Expected Thinking event"),
        }
    }

    #[test]
    fn test_parse_tool_use_event() {
        let json = r#"{"type":"tool_use","name":"bash","input":{"command":"ls"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ToolUse { name, input } => {
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            _ => panic!("Expected ToolUse event"),
        }
    }

    #[test]
    fn test_parse_message_event() {
        let json = r#"{"type":"message","content":"Task completed"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Message { content } => {
                assert_eq!(content, "Task completed");
            }
            _ => panic!("Expected Message event"),
        }
    }

    #[test]
    fn test_parse_complete_event() {
        let json = r#"{"type":"complete"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Complete));
    }

    #[test]
    fn test_parse_error_event() {
        let json = r#"{"type":"error","message":"Something went wrong"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Error { message } => {
                assert_eq!(message, "Something went wrong");
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[test]
    fn test_stream_reader_with_json() {
        let input = r#"{"type":"thinking","content":"test"}
{"type":"complete"}
"#;
        let cursor = Cursor::new(input);
        let mut stream = EventStream::new(cursor);

        let outputs = stream.read_all().unwrap();
        assert_eq!(outputs.len(), 2);

        match &outputs[0] {
            StreamOutput::Event(ClaudeEvent::Thinking { content }) => {
                assert_eq!(content, "test");
            }
            _ => panic!("Expected Thinking event"),
        }

        match &outputs[1] {
            StreamOutput::Event(ClaudeEvent::Complete) => {}
            _ => panic!("Expected Complete event"),
        }
    }

    #[test]
    fn test_stream_reader_with_mixed_content() {
        let input =
            "Regular output line\n{\"type\":\"thinking\",\"content\":\"test\"}\nAnother line\n";
        let cursor = Cursor::new(input);
        let mut stream = EventStream::new(cursor);

        let outputs = stream.read_all().unwrap();
        assert_eq!(outputs.len(), 3);

        assert!(matches!(outputs[0], StreamOutput::RawLine(_)));
        assert!(matches!(outputs[1], StreamOutput::Event(_)));
        assert!(matches!(outputs[2], StreamOutput::RawLine(_)));
    }

    #[test]
    fn test_stream_reader_with_malformed_json() {
        let input = "{\"type\":\"invalid\"\nNot JSON\n";
        let cursor = Cursor::new(input);
        let mut stream = EventStream::new(cursor);

        let outputs = stream.read_all().unwrap();
        assert_eq!(outputs.len(), 2);

        // Both should be treated as raw lines since they're malformed/invalid
        assert!(matches!(outputs[0], StreamOutput::RawLine(_)));
        assert!(matches!(outputs[1], StreamOutput::RawLine(_)));
    }

    #[test]
    fn test_stream_reader_with_empty_lines() {
        let input = "\n\n{\"type\":\"complete\"}\n\n";
        let cursor = Cursor::new(input);
        let mut stream = EventStream::new(cursor);

        let outputs = stream.read_all().unwrap();
        // Empty lines are preserved as RawLine
        assert_eq!(outputs.len(), 4);
    }
}
