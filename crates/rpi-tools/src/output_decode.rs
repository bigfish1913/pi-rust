//! Incremental text decoding for child-process output.
//!
//! Output arrives in arbitrary read-sized chunks, so a multi-byte character can
//! straddle two reads; decoding each chunk on its own turns it into `U+FFFD`.
//! [`StreamDecoder`] holds an incomplete trailing character until the next
//! chunk completes it.
//!
//! The encoding is chosen per line: a line that is valid UTF-8 is decoded as
//! UTF-8, anything else in a fallback encoding. On Windows the fallback is the
//! console code page (GBK/936 on a Chinese system, where `ipconfig`, `cmd`,
//! Python and Windows PowerShell write in that code page while Git and Node
//! write UTF-8, often within one command); elsewhere it is lossy UTF-8. `\n`
//! is never part of a multi-byte character in UTF-8 or in the legacy CJK
//! encodings, so it is a safe split point.

use encoding_rs::{Decoder, Encoding};

/// Decodes one output stream chunk by chunk. Create one per stream.
pub struct StreamDecoder {
    fallback: &'static Encoding,
    /// Bytes of the current line that are still valid (possibly incomplete)
    /// UTF-8 and not yet emitted.
    pending: Vec<u8>,
    /// Set once the current line turned out not to be UTF-8; the rest of the
    /// line goes through it, so it carries an incomplete character itself.
    legacy: Option<Decoder>,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// Decoder whose fallback is the platform's console encoding.
    pub fn new() -> Self {
        Self::with_fallback(console_encoding())
    }

    /// Decoder that decodes lines which are not UTF-8 in `fallback`.
    pub fn with_fallback(fallback: &'static Encoding) -> Self {
        Self {
            fallback,
            pending: Vec::new(),
            legacy: None,
        }
    }

    /// Decode as much of `bytes` (after anything held back) as is complete.
    pub fn push(&mut self, mut bytes: &[u8]) -> String {
        let mut out = String::new();
        while !bytes.is_empty() {
            let (line, ends_line) = match bytes.iter().position(|&b| b == b'\n') {
                Some(nl) => (&bytes[..=nl], true),
                None => (bytes, false),
            };
            bytes = &bytes[line.len()..];
            if let Some(decoder) = self.legacy.as_mut() {
                decode_legacy(decoder, line, ends_line, &mut out);
                if ends_line {
                    self.legacy = None;
                }
                continue;
            }
            self.pending.extend_from_slice(line);
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                }
                Err(e) if e.error_len().is_none() && !ends_line => {
                    // A UTF-8 character still being written: emit up to it.
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or_default());
                    self.pending.drain(..valid);
                }
                Err(_) => {
                    let mut decoder = self.fallback.new_decoder_without_bom_handling();
                    let line = std::mem::take(&mut self.pending);
                    decode_legacy(&mut decoder, &line, ends_line, &mut out);
                    if !ends_line {
                        self.legacy = Some(decoder);
                    }
                }
            }
        }
        out
    }

    /// Decode whatever is still held back; call once at end of stream.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if let Some(mut decoder) = self.legacy.take() {
            decode_legacy(&mut decoder, &[], true, &mut out);
        } else if !self.pending.is_empty() {
            let mut decoder = self.fallback.new_decoder_without_bom_handling();
            let rest = std::mem::take(&mut self.pending);
            decode_legacy(&mut decoder, &rest, true, &mut out);
        }
        out
    }
}

/// Decode a whole buffer at once (e.g. a probe's captured stdout).
pub fn decode_output(bytes: &[u8]) -> String {
    let mut decoder = StreamDecoder::new();
    let mut out = decoder.push(bytes);
    out.push_str(&decoder.finish());
    out
}

fn decode_legacy(decoder: &mut Decoder, bytes: &[u8], last: bool, out: &mut String) {
    let room = decoder
        .max_utf8_buffer_length(bytes.len())
        .unwrap_or(bytes.len() * 3 + 16);
    out.reserve(room);
    let _ = decoder.decode_to_string(bytes, out, last);
}

/// The encoding console programs write in.
#[cfg(windows)]
pub fn console_encoding() -> &'static Encoding {
    static ENCODING: std::sync::OnceLock<&'static Encoding> = std::sync::OnceLock::new();
    *ENCODING.get_or_init(|| {
        console_code_page()
            .and_then(codepage::to_encoding)
            .unwrap_or(encoding_rs::UTF_8)
    })
}

/// The encoding console programs write in.
#[cfg(not(windows))]
pub fn console_encoding() -> &'static Encoding {
    encoding_rs::UTF_8
}

/// The console output code page, read from `chcp` (a child inherits our
/// console, or gets a new one in the OEM code page — the same page our own
/// children write in). Spawning keeps this crate free of `unsafe` FFI, as
/// `taskkill` does for process trees; it runs once, on the first line that
/// is not UTF-8.
#[cfg(windows)]
fn console_code_page() -> Option<u16> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let out = std::process::Command::new("cmd")
        .args(["/d", "/c", "chcp"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_chcp(&out.stdout)
}

/// The number at the end of `chcp`'s localized "Active code page: 936".
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_chcp(stdout: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(stdout);
    let digits: String = text
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_in_reads(decoder: &mut StreamDecoder, bytes: &[u8], read: usize) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(read) {
            out.push_str(&decoder.push(chunk));
        }
        out.push_str(&decoder.finish());
        out
    }

    #[test]
    fn utf8_character_split_across_reads_survives() {
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(
            decode_in_reads(&mut decoder, "中文 ok\n".as_bytes(), 1),
            "中文 ok\n"
        );
    }

    #[test]
    fn gbk_character_split_across_reads_survives() {
        // "a中文" in GBK, one byte per read.
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        let bytes = [b'a', 0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_in_reads(&mut decoder, &bytes, 1), "a中文");
    }

    #[test]
    fn each_line_picks_its_own_encoding() {
        let mut bytes = "前\n".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4, b'\n']);
        bytes.extend_from_slice("后\n".as_bytes());
        let mut decoder = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(decode_in_reads(&mut decoder, &bytes, 3), "前\n中文\n后\n");
    }

    #[test]
    fn partial_line_is_emitted_before_its_newline() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push(b"progress 50%"), "progress 50%");
        assert_eq!(decoder.push(b"\n"), "\n");
    }

    #[test]
    fn chcp_output_yields_the_code_page() {
        assert_eq!(parse_chcp(b"Active code page: 936\r\n"), Some(936));
        assert_eq!(parse_chcp("活动代码页: 65001\r\n".as_bytes()), Some(65001));
    }
}
