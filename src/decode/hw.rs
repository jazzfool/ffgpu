#[cfg(target_os = "windows")]
mod d3d11va;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "macos")]
mod video_toolbox;

use crate::error::Result;
use ffmpeg_next::sys as ff;
use std::ptr::NonNull;

pub(crate) trait HardwareDecoder: Sized {
    const DEVICE_TYPE: ff::AVHWDeviceType;

    unsafe fn new(hwctx: NonNull<ff::AVBufferRef>) -> Result<Self>;
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        layout: &wgpu::BindGroupLayout,
    ) -> Result<()>;
    fn bind_group(&self) -> Option<&wgpu::BindGroup>;
    fn name(&self) -> &'static str;
}

#[cfg(target_os = "windows")]
pub(crate) type NativeDecoder = d3d11va::D3D11VAHardwareDecoder;

#[cfg(target_os = "linux")]
pub(crate) type NativeDecoder = vaapi::VAAPIHardwareDecoder;

#[cfg(target_os = "macos")]
pub(crate) type NativeDecoder = video_toolbox::VideoToolboxHardwareDecoder;
