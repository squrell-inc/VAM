use bytes::Bytes;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: [u8; 2] = *b"VA";
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024; // 64 KiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Stdin = 0x01,
    Stdout = 0x02,
    Stderr = 0x03,
    ControlGrpc = 0x04,
    StreamStorage = 0x06,
    StreamEgress = 0x07,
    StreamInference = 0x08,
}

impl FrameType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x01 => Some(FrameType::Stdin),
            0x02 => Some(FrameType::Stdout),
            0x03 => Some(FrameType::Stderr),
            0x04 => Some(FrameType::ControlGrpc),
            0x06 => Some(FrameType::StreamStorage),
            0x07 => Some(FrameType::StreamEgress),
            0x08 => Some(FrameType::StreamInference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub correlation_id: u32,
    pub payload: Bytes,
}

impl Frame {
    pub async fn read_from<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let mut header = [0u8; 11];
        reader.read_exact(&mut header).await?;

        if header[0..2] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid frame magic prefix",
            ));
        }

        let frame_type_raw = header[2];
        let frame_type = FrameType::from_u8(frame_type_raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid frame type: 0x{:02x}", frame_type_raw),
            )
        })?;

        let payload_len = u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
        let correlation_id = u32::from_be_bytes([header[7], header[8], header[9], header[10]]);

        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Payload size {} exceeds maximum 64 KiB limit", payload_len),
            ));
        }

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload).await?;
        }

        Ok(Frame {
            frame_type,
            correlation_id,
            payload: Bytes::from(payload),
        })
    }

    pub async fn write_to<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        if self.payload.len() > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Payload size {} exceeds maximum 64 KiB limit",
                    self.payload.len()
                ),
            ));
        }

        let mut header = [0u8; 11];
        header[0..2].copy_from_slice(&MAGIC);
        header[2] = self.frame_type as u8;
        header[3..7].copy_from_slice(&(self.payload.len() as u32).to_be_bytes());
        header[7..11].copy_from_slice(&self.correlation_id.to_be_bytes());

        writer.write_all(&header).await?;
        if !self.payload.is_empty() {
            writer.write_all(&self.payload).await?;
        }
        writer.flush().await?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    pub seq: u32,
    pub end_of_message: bool,
    pub payload: Bytes,
}

impl StreamChunk {
    pub fn encode(&self) -> Bytes {
        let mut buf = Vec::with_capacity(5 + self.payload.len());
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.push(if self.end_of_message { 1 } else { 0 });
        buf.extend_from_slice(&self.payload);
        Bytes::from(buf)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "StreamChunk too short, less than 5 bytes",
            ));
        }
        let seq = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let end_of_message = bytes[4] != 0;
        let payload = Bytes::copy_from_slice(&bytes[5..]);
        Ok(StreamChunk {
            seq,
            end_of_message,
            payload,
        })
    }
}

pub struct MessageReassembler {
    max_message_size: usize,
    // correlation_id -> (next_expected_seq, accumulated_payload)
    sessions: std::collections::HashMap<u32, (u32, Vec<u8>)>,
}

impl MessageReassembler {
    pub fn new(max_message_size: usize) -> Self {
        Self {
            max_message_size,
            sessions: std::collections::HashMap::new(),
        }
    }

    /// Process a new Frame. If the message is fully reassembled, returns Ok(Some(assembled_payload)).
    /// If the message is still being assembled, returns Ok(None).
    /// If an error occurs (e.g. sequence mismatch, size limit exceeded), returns Err(io::Error).
    pub fn handle_frame(&mut self, frame: &Frame) -> io::Result<Option<Bytes>> {
        let chunk = StreamChunk::decode(&frame.payload)?;
        let correlation_id = frame.correlation_id;

        let (expected_seq, buf) = self
            .sessions
            .entry(correlation_id)
            .or_insert_with(|| (0, Vec::new()));

        if chunk.seq != *expected_seq {
            let actual = chunk.seq;
            let expected = *expected_seq;
            self.sessions.remove(&correlation_id);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Out-of-order chunk for correlation ID {}: expected seq {}, got {}",
                    correlation_id, expected, actual
                ),
            ));
        }

        if buf.len() + chunk.payload.len() > self.max_message_size {
            self.sessions.remove(&correlation_id);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Assembled message size exceeds maximum limit of {} bytes",
                    self.max_message_size
                ),
            ));
        }

        buf.extend_from_slice(&chunk.payload);
        *expected_seq += 1;

        if chunk.end_of_message {
            let (_, final_buf) = self.sessions.remove(&correlation_id).unwrap();
            Ok(Some(Bytes::from(final_buf)))
        } else {
            Ok(None)
        }
    }

    pub fn remove_session(&mut self, correlation_id: u32) {
        self.sessions.remove(&correlation_id);
    }
}

pub fn split_payload(payload: &[u8], correlation_id: u32, frame_type: FrameType) -> Vec<Frame> {
    let chunk_size = MAX_PAYLOAD_SIZE - 5;
    if payload.is_empty() {
        let chunk = StreamChunk {
            seq: 0,
            end_of_message: true,
            payload: Bytes::new(),
        };
        return vec![Frame {
            frame_type,
            correlation_id,
            payload: chunk.encode(),
        }];
    }

    let mut frames = Vec::new();
    let mut seq = 0u32;
    let chunks_iter = payload.chunks(chunk_size);
    let total_chunks = chunks_iter.len();
    #[allow(clippy::explicit_counter_loop)] // seq is a u32 wire counter; kept as-is
    for (i, chunk_slice) in chunks_iter.enumerate() {
        let end_of_message = i == total_chunks - 1;
        let chunk = StreamChunk {
            seq,
            end_of_message,
            payload: Bytes::copy_from_slice(chunk_slice),
        };
        frames.push(Frame {
            frame_type,
            correlation_id,
            payload: chunk.encode(),
        });
        seq += 1;
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_frame_encode_decode() {
        let original_frame = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id: 1337,
            payload: Bytes::from_static(b"hello world"),
        };

        let mut buf = Vec::new();
        original_frame.write_to(&mut buf).await.unwrap();

        let mut reader = Cursor::new(buf);
        let decoded = Frame::read_from(&mut reader).await.unwrap();

        assert_eq!(original_frame, decoded);

        // Test StreamEgress round-trip
        let egress_frame = Frame {
            frame_type: FrameType::StreamEgress,
            correlation_id: 42,
            payload: Bytes::from_static(b"egress payload"),
        };
        let mut buf2 = Vec::new();
        egress_frame.write_to(&mut buf2).await.unwrap();
        let mut reader2 = Cursor::new(buf2);
        let decoded2 = Frame::read_from(&mut reader2).await.unwrap();
        assert_eq!(egress_frame, decoded2);
    }

    #[tokio::test]
    async fn test_frame_invalid_magic() {
        let buf = vec![0u8; 11];
        let mut reader = Cursor::new(buf);
        assert!(Frame::read_from(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn test_frame_oversized_payload() {
        let mut buf = [0u8; 11];
        buf[0..2].copy_from_slice(&MAGIC);
        buf[2] = FrameType::ControlGrpc as u8;
        // payload size = 65 KiB
        buf[3..7].copy_from_slice(&(65 * 1024u32).to_be_bytes());

        let mut reader = Cursor::new(buf);
        assert!(Frame::read_from(&mut reader).await.is_err());
    }

    #[test]
    fn test_chunk_encode_decode() {
        let original_chunk = StreamChunk {
            seq: 42,
            end_of_message: true,
            payload: Bytes::from_static(b"chunk data"),
        };
        let encoded = original_chunk.encode();
        let decoded = StreamChunk::decode(&encoded).unwrap();
        assert_eq!(original_chunk, decoded);
    }

    #[test]
    fn test_split_and_reassemble() {
        let data = vec![0u8; 150_000]; // ~150 KiB, should split into 3 chunks
        let correlation_id = 999;
        let frames = split_payload(&data, correlation_id, FrameType::StreamEgress);
        assert_eq!(frames.len(), 3);

        let mut reassembler = MessageReassembler::new(200_000);
        let mut result = None;
        for frame in &frames {
            assert_eq!(frame.frame_type, FrameType::StreamEgress);
            assert_eq!(frame.correlation_id, correlation_id);
            if let Some(payload) = reassembler.handle_frame(frame).unwrap() {
                result = Some(payload);
            }
        }

        let assembled = result.expect("Should have assembled fully");
        assert_eq!(assembled.len(), 150_000);
        assert_eq!(assembled.as_ref(), data.as_slice());
    }

    #[test]
    fn test_reassemble_out_of_order() {
        let data = vec![0u8; 150_000];
        let correlation_id = 999;
        let mut frames = split_payload(&data, correlation_id, FrameType::StreamEgress);

        // Swap chunk 0 and chunk 1 to simulate out-of-order arrival
        frames.swap(0, 1);

        let mut reassembler = MessageReassembler::new(200_000);
        let err = reassembler.handle_frame(&frames[0]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_reassemble_oversized() {
        let data = vec![0u8; 150_000];
        let correlation_id = 999;
        let frames = split_payload(&data, correlation_id, FrameType::StreamEgress);

        // Max size is 100 KiB, payload is 150 KiB
        let mut reassembler = MessageReassembler::new(100_000);

        let mut got_err = false;
        for frame in &frames {
            if let Err(err) = reassembler.handle_frame(frame) {
                assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                assert!(err.to_string().contains("exceeds maximum limit"));
                got_err = true;
                break;
            }
        }
        assert!(got_err);
    }
}
