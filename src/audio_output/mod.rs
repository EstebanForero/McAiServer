use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

pub mod speaker_output;
pub mod udp_output;

/// A trait for asynchronous audio output sinks.
/// Implementers will receive chunks of `Vec<i16>` (raw PCM samples) for playback or transmission.
#[async_trait]
pub trait AsyncAudioOutput: Send + Sync {
    /// Starts the audio output stream.
    /// `sample_rate` and `channels` are the parameters of the audio to be output.
    /// Returns a `Sender` that can be used to send chunks of PCM audio data (`Vec<i16>`).
    async fn start_stream(&mut self, sample_rate: u32, channels: u16) -> Result<Sender<Vec<i16>>>;

    /// Stops the audio output stream and cleans up resources.
    async fn stop_stream(&mut self) -> Result<()>;
}
