pub mod context;
pub mod decode;
pub mod playback;
pub mod video;

pub fn required_wgpu_device_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    match adapter.get_info().backend {
        wgpu::Backend::Vulkan => {
            wgpu::Features::TEXTURE_FORMAT_NV12 | wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32
        }
        wgpu::Backend::Dx12 => wgpu::Features::TEXTURE_FORMAT_NV12,
        _ => wgpu::Features::empty(),
    }
}
