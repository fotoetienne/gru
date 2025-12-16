use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum buffer size in characters before forcing a flush.
/// This prevents unbounded memory growth from streaming text without newlines.
/// 1000 characters is roughly 15-20 lines of typical code or prose.
const MAX_BUFFER_SIZE: usize = 1000;

/// Time-based text buffer for grouping streaming text fragments
/// Flushes on: newline characters, sentence boundaries (. ! ?), timeout, or buffer full
pub struct TextBuffer {
    buffer: Arc<Mutex<BufferState>>,
}

struct BufferState {
    text: String,
    last_update: Option<Instant>,
    flush_interval: Duration,
}

impl TextBuffer {
    /// Create a new TextBuffer with the specified flush interval
    pub fn new(flush_interval: Duration) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(BufferState {
                text: String::new(),
                last_update: None,
                flush_interval,
            })),
        }
    }

    /// Add text to the buffer
    /// Returns Some(flushed_text) if the buffer should be flushed, None otherwise
    pub fn add(&self, text: &str) -> Option<String> {
        let mut state = self
            .buffer
            .lock()
            .expect("TextBuffer mutex poisoned - indicates a panic in buffer code");

        // Check if we should flush due to timeout
        let should_flush_timeout = if let Some(last_update) = state.last_update {
            last_update.elapsed() >= state.flush_interval
        } else {
            false
        };

        // If timeout expired and we have buffered text, flush at word boundary
        if should_flush_timeout && !state.text.is_empty() {
            // Flush at last word boundary to avoid mid-word breaks
            let flushed = Self::flush_at_word_boundary(&mut state.text);
            state.text.push_str(text);
            // Start timing new buffer with the remaining text + just-added text
            state.last_update = Some(Instant::now());
            return Some(flushed);
        }

        // Add new text to buffer
        state.text.push_str(text);
        // Update or initialize the timestamp
        if state.last_update.is_none() {
            state.last_update = Some(Instant::now());
        }

        // Check if newly added text contains newline or buffer ends with sentence boundary
        // (more efficient than scanning entire buffer)
        let has_newline = text.contains('\n');
        // Check the full buffer state for sentence boundaries to ensure consistent behavior
        // regardless of how the stream is chunked. This handles cases where ". " arrives
        // split across multiple add() calls (e.g., "text." followed by " more text").
        let has_sentence_end =
            state.text.ends_with(". ") || state.text.ends_with("! ") || state.text.ends_with("? ");

        // Flush if newline or sentence boundary (natural boundaries)
        if has_newline || has_sentence_end {
            let flushed = std::mem::take(&mut state.text);
            // Reset timestamp since buffer is now empty
            state.last_update = None;
            Some(flushed)
        } else if state.text.len() > MAX_BUFFER_SIZE {
            // Buffer is full - flush at word boundary to avoid mid-word breaks
            let flushed = Self::flush_at_word_boundary(&mut state.text);
            // Reset timestamp and keep remaining partial word in buffer
            state.last_update = if state.text.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
            Some(flushed)
        } else {
            None
        }
    }

    /// Flush buffer at the last word boundary (space or newline)
    /// Returns flushed text, leaving any partial word in the buffer
    ///
    /// # UTF-8 Safety
    /// This function is UTF-8 safe because:
    /// - `rfind()` returns byte indices at UTF-8 character boundaries
    /// - We use `char_indices()` to find the next character boundary after the whitespace
    /// - All string slicing operations occur at valid UTF-8 boundaries
    fn flush_at_word_boundary(buffer: &mut String) -> String {
        // Find last space or newline to avoid breaking mid-word
        if let Some(last_boundary) = buffer.rfind(|c: char| c.is_whitespace()) {
            // Find the start of the next character after the whitespace boundary
            // This ensures we don't split multi-byte UTF-8 characters
            let next_char_start = buffer[last_boundary..]
                .char_indices()
                .nth(1)
                .map(|(idx, _)| last_boundary + idx)
                .unwrap_or(buffer.len());

            // Split at the safe boundary, keeping the partial word in the buffer
            let flushed = buffer[..next_char_start].to_string();
            *buffer = buffer[next_char_start..].to_string();
            flushed
        } else {
            // No word boundary found - flush everything to avoid blocking
            // This can happen with very long words or non-text content
            std::mem::take(buffer)
        }
    }

    /// Force flush the buffer, returning any accumulated text
    pub fn flush(&self) -> Option<String> {
        let mut state = self
            .buffer
            .lock()
            .expect("TextBuffer mutex poisoned - indicates a panic in buffer code");

        if state.text.is_empty() {
            None
        } else {
            let flushed = std::mem::take(&mut state.text);
            state.last_update = None;
            Some(flushed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_buffer_accumulates_text() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Add text without triggering flush
        let result = buffer.add("Hello");
        assert!(result.is_none());

        let result = buffer.add(" world");
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_flushes_on_newline() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        let result = buffer.add(" world\n");

        assert_eq!(result, Some("Hello world\n".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_timeout() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text with a space to create a word boundary
        let result = buffer.add("Hello ");
        assert!(result.is_none());

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Next add should flush due to timeout at word boundary
        let result = buffer.add("world");
        assert_eq!(result, Some("Hello ".to_string()));
    }

    #[test]
    fn test_manual_flush() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        buffer.add(" world");

        let result = buffer.flush();
        assert_eq!(result, Some("Hello world".to_string()));

        // Second flush should return None
        let result = buffer.flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_flushes_when_full() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Add text that exceeds MAX_BUFFER_SIZE
        let long_text = "a".repeat(MAX_BUFFER_SIZE + 1);
        let result = buffer.add(&long_text);

        // Should flush immediately when buffer is too full
        assert!(result.is_some());
    }

    #[test]
    fn test_multiple_newlines() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        let result = buffer.add("Line 1\nLine 2\n");
        assert_eq!(result, Some("Line 1\nLine 2\n".to_string()));

        // Buffer should be empty now
        let result = buffer.flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_flushes_on_period_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        let result = buffer.add(" world. ");

        assert_eq!(result, Some("Hello world. ".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_question_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Is this working");
        let result = buffer.add("? ");

        assert_eq!(result, Some("Is this working? ".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_exclamation_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Great work");
        let result = buffer.add("! ");

        assert_eq!(result, Some("Great work! ".to_string()));
    }

    #[test]
    fn test_url_does_not_trigger_flush() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // URLs like "example.com" should not trigger flush (no space after period)
        let result = buffer.add("Visit example.com");
        assert!(result.is_none());

        // Verify buffer still contains the text
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("Visit example.com".to_string()));
    }

    #[test]
    fn test_abbreviation_triggers_flush_acceptable() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // "Dr." followed by space triggers flush even though it's an abbreviation.
        // This is acceptable per issue #106 - we prioritize simplicity and performance
        // over handling all edge cases. The timeout will eventually flush anyway.
        let result = buffer.add("Dr. ");
        assert_eq!(result, Some("Dr. ".to_string()));
    }

    #[test]
    fn test_multiple_sentence_boundaries() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // First sentence
        let result = buffer.add("First sentence. ");
        assert_eq!(result, Some("First sentence. ".to_string()));

        // Second sentence
        let result = buffer.add("Second sentence. ");
        assert_eq!(result, Some("Second sentence. ".to_string()));

        // Buffer should be empty now
        let result = buffer.flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_sentence_boundary_without_trailing_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Period without space should not trigger flush
        let result = buffer.add("Hello world.");
        assert!(result.is_none());

        // Verify buffer still contains the text
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("Hello world.".to_string()));
    }

    #[test]
    fn test_sentence_boundary_split_across_fragments() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Add text with period but no space
        let result = buffer.add("Hello world.");
        assert!(result.is_none());

        // Add space - should trigger flush because buffer now ends with ". "
        let result = buffer.add(" ");
        assert_eq!(result, Some("Hello world. ".to_string()));

        // Buffer should be empty now
        let flushed = buffer.flush();
        assert!(flushed.is_none());
    }

    #[test]
    fn test_timeout_flushes_at_word_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text that contains multiple words
        let result = buffer.add("The quick brown");
        assert!(result.is_none());

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush up to last space, keeping "brown" in buffer
        let result = buffer.add(" fox");
        assert_eq!(result, Some("The quick ".to_string()));

        // Verify "brown fox" is still in buffer
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("brown fox".to_string()));
    }

    #[test]
    fn test_timeout_with_no_word_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text without any spaces (no word boundary)
        let result = buffer.add("Supercalifragilisticexpialidocious");
        assert!(result.is_none());

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush everything since no word boundary exists
        let result = buffer.add("text");
        assert_eq!(
            result,
            Some("Supercalifragilisticexpialidocious".to_string())
        );

        // Verify new text is in buffer
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("text".to_string()));
    }

    #[test]
    fn test_buffer_full_flushes_at_word_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Create text that will exceed MAX_BUFFER_SIZE when combined
        // Each repetition is 5 characters ("word "), so repeat enough to exceed MAX_BUFFER_SIZE
        let long_text = "word ".repeat((MAX_BUFFER_SIZE / 5) + 100);

        // Add text that exceeds MAX_BUFFER_SIZE
        let result = buffer.add(&long_text);

        // Should flush at word boundary (not mid-word)
        assert!(result.is_some());
        let flushed = result.unwrap();

        // Verify flushed text doesn't end mid-word (should end with space)
        assert!(
            flushed.ends_with(' ') || flushed.ends_with('\n'),
            "Flushed text should end at word boundary, got: '{}'",
            &flushed[flushed.len().saturating_sub(20)..]
        );
    }

    #[test]
    fn test_no_mid_word_breaks_on_partial_fragments() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Simulate LLM token stream that might break mid-word
        buffer.add("Comple");
        buffer.add("xity Asse");

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush at word boundary
        let result = buffer.add("ssment");

        // Should flush "Complexity " (up to last space), keeping "Assessment" in buffer
        assert_eq!(result, Some("Complexity ".to_string()));

        // Verify remaining text
        let remaining = buffer.flush();
        assert_eq!(remaining, Some("Assessment".to_string()));
    }

    #[test]
    fn test_multibyte_unicode_after_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text with multi-byte Unicode characters (Chinese) after a space
        buffer.add("Hello 世界");

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush at word boundary without breaking Unicode
        let result = buffer.add("test");
        assert_eq!(result, Some("Hello ".to_string()));

        // Verify the multi-byte characters were kept intact
        let remaining = buffer.flush();
        assert_eq!(remaining, Some("世界test".to_string()));
    }

    #[test]
    fn test_emoji_after_word_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text with emoji (multi-byte UTF-8) after a space
        buffer.add("Great work 🎉");

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush at word boundary without breaking emoji
        let result = buffer.add(" more");
        assert_eq!(result, Some("Great work ".to_string()));

        // Verify emoji was kept intact
        let remaining = buffer.flush();
        assert_eq!(remaining, Some("🎉 more".to_string()));
    }

    #[test]
    fn test_unicode_whitespace_boundary() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Use non-breaking space (U+00A0) - a Unicode whitespace character
        buffer.add("Hello\u{00A0}world");

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush at the non-breaking space
        let result = buffer.add("!");
        assert_eq!(result, Some("Hello\u{00A0}".to_string()));

        // Verify remaining text
        let remaining = buffer.flush();
        assert_eq!(remaining, Some("world!".to_string()));
    }

    #[test]
    fn test_flush_when_ending_with_whitespace() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text ending with space
        buffer.add("Hello ");

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Add more text - should flush everything up to and including the space
        let result = buffer.add("world");
        assert_eq!(result, Some("Hello ".to_string()));

        // Verify remaining text
        let remaining = buffer.flush();
        assert_eq!(remaining, Some("world".to_string()));
    }
}
