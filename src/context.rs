pub(crate) mod pipeline_cache;

use crate::{decode::audio::AudioSink, error::Result, video::Video};
use pipeline_cache::PipelineCache;
use std::path::Path;

pub struct Context {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline_cache: PipelineCache,
}

impl Context {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Self {
        let instance = instance.clone();
        let adapter = adapter.clone();
        let device = device.clone();
        let queue = queue.clone();

        let pipeline_cache = PipelineCache::new(device.clone());

        Context {
            instance,
            adapter,
            device,
            queue,
            pipeline_cache,
        }
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
            &mut self.pipeline_cache,
            path,
        )
    }
}
