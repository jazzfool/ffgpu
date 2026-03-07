pub mod context;
pub mod decode;
pub mod error;
pub mod playback;
pub mod video;

#[cfg(target_os = "windows")]
pub fn required_wgpu_device_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    match adapter.get_info().backend {
        wgpu::Backend::Vulkan => {
            wgpu::Features::TEXTURE_FORMAT_NV12 | wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32
        }
        wgpu::Backend::Dx12 => wgpu::Features::TEXTURE_FORMAT_NV12,
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
