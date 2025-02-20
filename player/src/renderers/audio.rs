use std::{future::Future, sync::Arc, time::Duration};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, StreamConfig, SupportedStreamConfig,
};

use ffmpeg_next::frame::Audio;
use pollster::FutureExt;
use tokio::{
    join,
    sync::{
        mpsc::{self, Sender},
        Notify,
    },
    task::JoinHandle,
};

pub struct AudioRenderer {
    sample_sender: Option<Sender<f32>>,
    device: Device,
    config: SupportedStreamConfig,
    play_handle: Option<JoinHandle<()>>,
    stop: Arc<Notify>,
}

impl AudioRenderer {
    async fn process_samples<F, Fut>(&self, frame: Audio, put_fn: F)
    where
        F: Fn(&AudioRenderer, Audio) -> Fut,
        Fut: Future<Output = ()>,
    {
        put_fn(self, frame).await;
    }
}

impl AudioRenderer {
    pub fn new() -> Self {
        let stop = Arc::new(Notify::new());

        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No output device")
            .unwrap();
        let config = device
            .default_output_config()
            .expect("Failed to get default config");

        AudioRenderer {
            sample_sender: None,
            device,
            config,
            play_handle: None,
            stop,
        }
    }

    pub async fn put_sample(&self, frame: Audio) {
        if let Some(sample_sender) = &self.sample_sender {
            let expected_bytes: usize =
                frame.samples() * frame.channels() as usize * std::mem::size_of::<f32>();

            let cpal_sample_data: &[f32] = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);

            for &sample in cpal_sample_data {
                let _ = sample_sender.send(sample).await;
            }
        }
    }

    pub fn sample_rate(&mut self) -> u32 {
        self.config.sample_rate().0
    }

    pub fn start(&mut self) {
        let (sample_sender, mut sample_receiver) = mpsc::channel::<f32>(8192); // Buffer size 8192
        self.sample_sender = Some(sample_sender);

        let stop_blocker = self.stop.clone();

        let device = self.device.clone();
        let config: SupportedStreamConfig = self.config.clone();

        let play_handle = tokio::spawn(async move {
            let stream_config = StreamConfig {
                channels: config.channels(),
                sample_rate: config.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            };

            let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    *sample = sample_receiver.try_recv().unwrap_or(Sample::EQUILIBRIUM);
                }
            };

            let err_fn = |err| eprintln!("an error occurred on stream: {}", err);

            let stream = device
                .build_output_stream(
                    &stream_config,
                    callback,
                    err_fn,
                    Some(Duration::from_secs(20)),
                )
                .expect("Failed to build audio stream");

            stream.play().expect("Failed to start audio stream");

            stop_blocker.notified().block_on();
        });

        self.play_handle = Some(play_handle);
    }

    pub async fn stop(&mut self) {
        self.stop.notify_waiters();

        if let Some(handle) = self.play_handle.take() {
            _ = join!(handle);
        }
    }
}
