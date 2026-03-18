pub(crate) mod context;
pub(crate) mod decode;
pub(crate) mod error;
pub(crate) mod video;

pub use context::Context;
pub use decode::{
    audio::{AudioMetadata, AudioParameters, AudioSink, DeviceAudioSink},
    video::VideoMetadata,
};
pub use error::{Error, Result};
pub use video::{SeekMode, Video};

#[cfg(target_os = "windows")]
pub fn required_wgpu_device_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    match adapter.get_info().backend {
        wgpu::Backend::Vulkan => {
            wgpu::Features::TEXTURE_FORMAT_NV12
                | wgpu::Features::TEXTURE_FORMAT_P010
                | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32
        }
        wgpu::Backend::Dx12 => {
            wgpu::Features::TEXTURE_FORMAT_NV12
                | wgpu::Features::TEXTURE_FORMAT_P010
                | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
        }
        _ => wgpu::Features::empty(),
    }
}

#[cfg(target_os = "macos")]
pub fn required_wgpu_device_features(_adapter: &wgpu::Adapter) -> wgpu::Features {
    wgpu::Features::empty()
}

#[cfg(target_os = "linux")]
pub fn required_wgpu_device_features(_adapter: &wgpu::Adapter) -> wgpu::Features {
    wgpu::Features::empty()
}
