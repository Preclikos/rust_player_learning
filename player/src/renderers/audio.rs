use std::{sync::Arc, time::Duration};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, StreamConfig, SupportedStreamConfig,
};

use ffmpeg_next::frame::Audio;
use pollster::FutureExt;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify, RwLock,
};

pub struct AudioRenderer {
    command_sender: Sender<AudioRendererCommand>,
    sample_sender: Sender<f32>,
    sample_rate: u32,
}

enum AudioRendererCommand {
    Stop,
    Volume(f32),
}

impl AudioRenderer {
    pub fn new() -> Self {
        let stop = Arc::new(Notify::new());

        let (command_sender, command_receiver) = mpsc::channel(32);
        let audio_thread = AudioRenderer::start_thread(command_receiver, stop);

        AudioRenderer {
            command_sender,
            sample_sender: audio_thread.0,
            sample_rate: audio_thread.1,
        }
    }

    async fn start_audio(
        mut sample_receiver: Receiver<f32>,
        device: Device,
        config: SupportedStreamConfig,
        volume: Arc<RwLock<f32>>,
        stop: Arc<Notify>,
    ) {
        let stream_config = StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let vol = volume.blocking_read();
            for sample in data.iter_mut() {
                let asample = sample_receiver.try_recv().unwrap_or(Sample::EQUILIBRIUM);
                *sample = asample * *vol;
            }
        };

        let err_fn = |err| eprintln!("An error occurred on stream: {}", err);

        let stream = device
            .build_output_stream(
                &stream_config,
                callback,
                err_fn,
                Some(Duration::from_secs(20)),
            )
            .expect("Failed to build audio stream");

        stream.play().expect("Failed to start audio stream");

        stop.notified().block_on();
    }

    fn start_thread(
        mut command_receiver: Receiver<AudioRendererCommand>,
        stop: Arc<Notify>,
    ) -> (Sender<f32>, u32) {
        let (sample_sender, sample_receiver) = mpsc::channel::<f32>(8192);

        let device = cpal::default_host()
            .default_output_device()
            .expect("No output device");

        let config: SupportedStreamConfig = device
            .default_output_config()
            .expect("Failed to get default config");

        let volume = Arc::new(RwLock::new(0.3_f32));

        let stop_cpal = stop.clone();
        let config_cpal = config.clone();
        let volume_cpal = Arc::clone(&volume);
        tokio::spawn(AudioRenderer::start_audio(
            sample_receiver,
            device,
            config_cpal,
            volume_cpal,
            stop_cpal,
        ));

        let volume_handle = Arc::clone(&volume);
        tokio::spawn(async move {
            while let Some(command) = command_receiver.recv().await {
                match command {
                    AudioRendererCommand::Stop => {
                        stop.notify_waiters();
                        break;
                    }
                    AudioRendererCommand::Volume(new_volume) => {
                        let mut vol = volume_handle.write().await;
                        *vol += new_volume;
                        println!("Volume changed to: {}", *vol);
                    }
                }
            }
        });

        (sample_sender, config.sample_rate().0)
    }

    pub async fn put_sample(&self, frame: Audio) {
        let expected_bytes =
            frame.samples() * frame.channels() as usize * std::mem::size_of::<f32>();
        let cpal_sample_data: &[f32] = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);

        for &sample in cpal_sample_data {
            let _ = self.sample_sender.send(sample).await;
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub async fn stop(&self) {
        _ = self.command_sender.send(AudioRendererCommand::Stop).await;
    }

    pub async fn volume(&self, volume_diff: f32) {
        _ = self
            .command_sender
            .send(AudioRendererCommand::Volume(volume_diff))
            .await;
    }
}
