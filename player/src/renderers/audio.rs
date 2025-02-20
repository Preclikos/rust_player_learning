use std::{sync::Arc, time::Duration};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, StreamConfig, SupportedStreamConfig,
};
use ffmpeg_next::frame::Audio;
use pollster::FutureExt;
use ringbuf::{
    storage::Heap,
    traits::{Consumer, Producer, Split},
    wrap::caching::Caching,
    HeapRb, SharedRb,
};
use tokio::sync::Notify;

pub struct AudioRenderer {
    sample_producer: Caching<Arc<SharedRb<Heap<f32>>>, true, false>,
    sample_consumer: Caching<Arc<SharedRb<Heap<f32>>>, false, true>,
    device: Device,
    config: SupportedStreamConfig,
    stop: Arc<Notify>,
}

impl AudioRenderer {
    pub fn new() -> Self {
        let stop = Arc::new(Notify::new());

        let buffer = HeapRb::<f32>::new(4096);
        let (sample_producer, sample_consumer) = buffer.split();

        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No output device")
            .unwrap();
        let config = device
            .default_output_config()
            .expect("Failed to get default config");

        AudioRenderer {
            sample_producer,
            sample_consumer,
            device,
            config,
            stop,
        }
    }
    pub fn start(&mut self) {
        let renderer = self;
        tokio::spawn(renderer.play());
    }

    pub async fn put_sample(&mut self, frame: Audio) {
        let expected_bytes: usize =
            frame.samples() * frame.channels() as usize * core::mem::size_of::<f32>();

        let cpal_sample_data: &[f32] = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);

        let mut remaining = cpal_sample_data;
        while !remaining.is_empty() {
            let written = self.sample_producer.push_slice(remaining);
            remaining = &remaining[written..];
            tokio::task::yield_now().await;
        }
    }

    async fn play(&mut self) {
        // Callback closure using the mutable reference
        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let filled = self.sample_consumer.as_mut().pop_slice(data);
            data[filled..].fill(Sample::EQUILIBRIUM); // Fill remaining data with equilibrium
        };

        let stream_config = StreamConfig {
            channels: self.config.channels(),
            sample_rate: self.config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let err_fn = |err| eprintln!("an error occurred on stream: {}", err);

        let stream = self
            .device
            .build_output_stream(
                &stream_config,
                callback,
                err_fn,
                Some(Duration::from_secs(20)),
            )
            .expect("Failed to build audio stream");

        stream.play().expect("Failed to start audio stream");

        self.stop.notified().block_on();
    }

    // Implement the stop function to notify the stop channel
    pub fn stop(&self) {
        self.stop.notify_waiters(); // Notify all waiters to stop
    }
}
