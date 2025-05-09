use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;

pub mod mic_input;
pub mod tcp_input;

#[async_trait]
pub trait AsyncAudioInput: Send + Sync {
    /// Starts the audio stream.
    /// Returns a `Receiver` that will yield chunks of PCM audio data (`Vec<i16>`).
    async fn start_stream(&mut self) -> Result<Receiver<Vec<i16>>>;

    /// Stops the audio stream and cleans up resources.
    async fn stop_stream(&mut self) -> Result<()>;

    /// Gets the sample rate of the audio stream.
    fn sample_rate(&self) -> u32;

    /// Gets the number of channels in the audio stream.
    fn channels(&self) -> u16;
}
