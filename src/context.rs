pub(crate) mod pipeline_cache;

use crate::video::Video;
use pipeline_cache::PipelineCache;

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

    pub fn create_video(&mut self) -> Video {
        Video::new(
            self.instance.clone(),
            self.adapter.clone(),
            self.device.clone(),
            self.queue.clone(),
            &mut self.pipeline_cache,
        )
    }
}
