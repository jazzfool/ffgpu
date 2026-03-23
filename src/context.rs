pub(crate) mod layout;
pub(crate) mod pipeline_cache;

use crate::{decode::audio::AudioSink, error::Result, video::Video};
use pipeline_cache::PipelineCache;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub struct Context {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline_cache: Arc<Mutex<PipelineCache>>,
}

impl Context {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Self> {
        ffmpeg_next::init()?;

        let instance = instance.clone();
        let adapter = adapter.clone();
        let device = device.clone();
        let queue = queue.clone();

        let pipeline_cache = Arc::new(Mutex::new(PipelineCache::new(device.clone())));

        Ok(Context {
            instance,
            adapter,
            device,
            queue,
            pipeline_cache,
        })
    }

    pub fn create_video<P>(&mut self, path: &P) -> Result<(Video, AudioSink)>
    where
        P: AsRef<Path> + ?Sized,
    {
        Video::new(
            self.instance.clone(),
            self.adapter.clone(),
            self.device.clone(),
            self.queue.clone(),
            self.pipeline_cache.clone(),
            path,
        )
    }
}
