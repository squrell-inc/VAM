use prost::Message;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use vam_spec::v1::{InferenceRequest, InferenceResponseChunk};
use vam_vsock::{split_payload, Frame, FrameType, StreamChunk};

type PendingStreams =
    Arc<Mutex<HashMap<u32, mpsc::Sender<Result<InferenceResponseChunk, String>>>>>;

pub struct InferenceClient {
    request_tx: mpsc::Sender<Frame>,
    pending_streams: PendingStreams,
    next_correlation_id: AtomicU32,
}

impl InferenceClient {
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (request_tx, mut request_rx) = mpsc::channel::<Frame>(128);
        let pending_streams = Arc::new(Mutex::new(HashMap::<
            u32,
            mpsc::Sender<Result<InferenceResponseChunk, String>>,
        >::new()));
        let pending_clone = pending_streams.clone();

        // Spawn writer loop
        tokio::spawn(async move {
            while let Some(frame) = request_rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("[inference-client] Writer error: {}", e);
                    break;
                }
            }
        });

        // Spawn reader loop
        tokio::spawn(async move {
            while let Ok(frame) = Frame::read_from(&mut reader).await {
                {
                    if frame.frame_type != FrameType::StreamInference {
                        continue;
                    }

                    let correlation_id = frame.correlation_id;
                    let chunk = match StreamChunk::decode(&frame.payload) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("[inference-client] Chunk decode error: {}", e);
                            continue;
                        }
                    };

                    let opt_tx = {
                        let map = pending_clone.lock().unwrap();
                        map.get(&correlation_id).cloned()
                    };

                    if let Some(tx) = opt_tx {
                        if chunk.end_of_message {
                            // Close the stream
                            if !chunk.payload.is_empty() {
                                if let Ok(resp_chunk) =
                                    InferenceResponseChunk::decode(chunk.payload.as_ref())
                                {
                                    let _ = tx.send(Ok(resp_chunk)).await;
                                }
                            }
                            let mut map = pending_clone.lock().unwrap();
                            map.remove(&correlation_id);
                        } else {
                            match InferenceResponseChunk::decode(chunk.payload.as_ref()) {
                                Ok(resp_chunk) => {
                                    if tx.send(Ok(resp_chunk)).await.is_err() {
                                        let mut map = pending_clone.lock().unwrap();
                                        map.remove(&correlation_id);
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(format!("Decode error: {}", e))).await;
                                    let mut map = pending_clone.lock().unwrap();
                                    map.remove(&correlation_id);
                                }
                            }
                        }
                    }
                }
            }
        });

        Self {
            request_tx,
            pending_streams,
            next_correlation_id: AtomicU32::new(1),
        }
    }

    pub async fn infer(
        &self,
        request: InferenceRequest,
    ) -> Result<mpsc::Receiver<Result<InferenceResponseChunk, String>>, String> {
        let cid = self.next_correlation_id.fetch_add(1, Ordering::SeqCst);
        let mut payload = Vec::new();
        request.encode(&mut payload).map_err(|e| e.to_string())?;

        let (tx, rx) = mpsc::channel(128);
        {
            let mut map = self.pending_streams.lock().unwrap();
            map.insert(cid, tx);
        }

        let frames = split_payload(&payload, cid, FrameType::StreamInference);
        for frame in frames {
            self.request_tx
                .send(frame)
                .await
                .map_err(|_| "Vsock channel closed".to_string())?;
        }

        Ok(rx)
    }
}
