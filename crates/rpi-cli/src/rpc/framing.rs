//! Bounded JSONL framing for the native RPC transport.
use super::protocol::MAX_FRAME_BYTES;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Read one bounded JSONL frame. The host drives reads through `lines()` today;
/// this stays as the bounded reader for callers that need the frame-size cap.
#[allow(dead_code)]
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        if line.len() + length > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RPC frame exceeds maximum size",
            ));
        }
        let complete = bytes[length - 1] == b'\n';
        line.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if complete {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_one_bounded_frame() {
        let mut reader = BufReader::new(&b"{\"type\":\"ping\"}\n"[..]);
        let frame = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame, b"{\"type\":\"ping\"}\n");
    }
}
