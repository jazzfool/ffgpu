pub mod software;

#[cfg(target_os = "windows")]
pub mod d3d11va;
#[cfg(target_os = "linux")]
pub mod vaapi;
#[cfg(target_os = "macos")]
pub mod video_toolbox;

use crate::{
    context::{layout, pipeline_cache::PipelineCache},
    error::Result,
};
use ffmpeg_next::sys as ff;
use std::ptr::NonNull;

// needs to be separate from FrameAdapater to be dyn compatible
pub(crate) trait FrameAdapterBuilder: FrameAdapter + Sized {
    unsafe fn new(decoder: NonNull<ff::AVCodecContext>) -> Result<Self>;
    fn supports_format(format: ff::AVPixelFormat) -> bool;
}

pub(crate) trait FrameAdapter {
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<()>;
    fn layout_identity(&self) -> Option<layout::FrameDescriptor<()>>;
    fn bind_group(&self) -> Option<&wgpu::BindGroup>;
    fn name(&self) -> &'static str;
}
