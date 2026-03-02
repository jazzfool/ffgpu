pub(crate) mod pipeline_cache;

use crate::video::Video;
use pipeline_cache::PipelineCache;

pub struct Context {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline_cache: PipelineCache,
}

impl Context {
    pub fn new(adapter: &wgpu::Adapter, device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let adapter = adapter.clone();
        let device = device.clone();
        let queue = queue.clone();

        let pipeline_cache = PipelineCache::new(device.clone());

        Context {
            adapter,
            device,
            queue,
            pipeline_cache,
        }
    }

    pub fn create_video(&mut self) -> Video {
        Video::new(
            self.adapter.clone(),
            self.device.clone(),
            self.queue.clone(),
            &mut self.pipeline_cache,
        )
    }
}
