use super::FrameAdapter;
use crate::{
    context::{layout, pipeline_cache::PipelineCache},
    decode::hw::FrameAdapterBuilder,
    error::{Error, Result},
};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{ffi::c_void, mem::ManuallyDrop, ptr::NonNull};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::{Direct3D11 as D3D11, Direct3D12 as D3D12, Dxgi},
    },
    core::Interface,
};

// see libavutil/hwcontext_d3d11va.h
// valid for ffmpeg 3.4 to 8.0 (asserted by AVUTIL_VERSION)
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: unsafe extern "C" fn(*mut c_void),
    unlock: unsafe extern "C" fn(*mut c_void),
    lock_ctx: *mut c_void,
}

fn get_dxgi_yuv_format(format: Dxgi::Common::DXGI_FORMAT) -> Option<wgpu::TextureFormat> {
    match format {
        Dxgi::Common::DXGI_FORMAT_NV12 => Some(wgpu::TextureFormat::NV12),
        Dxgi::Common::DXGI_FORMAT_P010 => Some(wgpu::TextureFormat::P010),
        _ => None,
    }
}

fn get_dxgi_layout(
    format: Dxgi::Common::DXGI_FORMAT,
) -> Option<layout::FrameDescriptor<wgpu::TextureFormat>> {
    match format {
        Dxgi::Common::DXGI_FORMAT_NV12 => Some(layout::FrameDescriptor {
            planes: layout::PlaneLayout::PackedYUV420([
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureFormat::Rg8Unorm,
            ]),
            depth: layout::Depth::D8,
        }),
        Dxgi::Common::DXGI_FORMAT_P010 => Some(layout::FrameDescriptor {
            planes: layout::PlaneLayout::PackedYUV420([
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            ]),
            depth: layout::Depth::D16,
        }),
        _ => None,
    }
}

struct ImportedTexture {
    // TODO(jazzfool): FFmpeg recently added the ability to specify SHARED misc flag (not yet released as of 02/26)
    // by passing "SHARED" in an AVDictionary to hwcontext_d3d11va
    // which means we could skip copying to another D3D11 with the SHARED flag and directly export the FFmpeg-created texture
    shared_texture: D3D11::ID3D11Texture2D,
    identity: layout::FrameDescriptor<()>,
    bg0: wgpu::BindGroup,
}

impl ImportedTexture {
    unsafe fn new_d3d12(
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
        color_space: ffn::color::Space,
    ) -> Result<Self> {
        if get_dxgi_yuv_format(desc.Format).is_none() {
            return Err(Error::UnsupportedPixelFormat);
        }

        unsafe {
            let (shared_texture, desc, handle) =
                ImportedTexture::create_shared_texture(d3d11_device, desc)?;
            let texture = ImportedTexture::import_handle_d3d12(device, &desc, handle)?;

            let layout = get_dxgi_layout(desc.Format).ok_or(Error::UnsupportedPixelFormat)?;

            let bg0 = pipeline_cache.bind_frame_textures(
                &layout::FrameDescriptor {
                    planes: layout::PlaneLayout::PackedYUV420([
                        texture.create_view(&wgpu::TextureViewDescriptor {
                            aspect: wgpu::TextureAspect::Plane0,
                            ..Default::default()
                        }),
                        texture.create_view(&wgpu::TextureViewDescriptor {
                            aspect: wgpu::TextureAspect::Plane1,
                            ..Default::default()
                        }),
                    ]),
                    depth: layout.depth,
                },
                color_space,
            );

            Ok(ImportedTexture {
                shared_texture,
                identity: layout.as_identity(),
                bg0,
            })
        }
    }

    unsafe fn new_vulkan(
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
        color_space: ffn::color::Space,
    ) -> Result<Self> {
        if get_dxgi_yuv_format(desc.Format).is_none() {
            return Err(Error::UnsupportedPixelFormat);
        }

        unsafe {
            let (shared_texture, desc, handle) =
                ImportedTexture::create_shared_texture(d3d11_device, desc)?;
            let texture = ImportedTexture::import_handle_vulkan(device, &desc, handle)?;

            let layout = get_dxgi_layout(desc.Format).ok_or(Error::UnsupportedPixelFormat)?;

            let bg0 = pipeline_cache.bind_frame_textures(
                &layout::FrameDescriptor {
                    planes: layout::PlaneLayout::PackedYUV420([
                        texture.create_view(&wgpu::TextureViewDescriptor {
                            aspect: wgpu::TextureAspect::Plane0,
                            ..Default::default()
                        }),
                        texture.create_view(&wgpu::TextureViewDescriptor {
                            aspect: wgpu::TextureAspect::Plane1,
                            ..Default::default()
                        }),
                    ]),
                    depth: layout.depth,
                },
                color_space,
            );

            Ok(ImportedTexture {
                shared_texture,
                identity: layout.as_identity(),
                bg0,
            })
        }
    }

    unsafe fn create_shared_texture(
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> Result<(D3D11::ID3D11Texture2D, D3D11::D3D11_TEXTURE2D_DESC, HANDLE)> {
        unsafe {
            let texture_desc = D3D11::D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11::D3D11_USAGE_DEFAULT,
                BindFlags: D3D11::D3D11_BIND_SHADER_RESOURCE.0 as _,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11::D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32)
                    | (D3D11::D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32),
            };
            let mut texture = None;
            d3d11_device
                .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                .map_err(|_| Error::TextureShare)?;
            let Some(texture) = texture else { panic!() };

            let dxgi_resource = texture.cast::<Dxgi::IDXGIResource1>().unwrap();
            let handle = dxgi_resource
                .CreateSharedHandle(None, Dxgi::DXGI_SHARED_RESOURCE_READ.0, None)
                .map_err(|_| Error::TextureShare)?;

            Ok((texture, texture_desc, handle))
        }
    }

    unsafe fn import_handle_d3d12(
        device: &wgpu::Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
        handle: HANDLE,
    ) -> Result<wgpu::Texture> {
        unsafe {
            let format =
                get_dxgi_yuv_format(desc.Format).unwrap(/* invariant by Self::new_d3d12 */);

            let hal_device = device.as_hal::<wgpu::hal::dx12::Api>().unwrap();
            let mut d3d12_texture: Option<D3D12::ID3D12Resource> = None;
            hal_device
                .raw_device()
                .OpenSharedHandle(handle, &mut d3d12_texture)
                .map_err(|_| Error::TextureShare)?;
            let Some(d3d12_texture) = d3d12_texture else {
                return Err(Error::TextureShare);
            };
            let hal_texture = wgpu::hal::dx12::Device::texture_from_raw(
                d3d12_texture,
                format,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width: desc.Width,
                    height: desc.Height,
                    depth_or_array_layers: 1,
                },
                1,
                1,
            );

            Ok(device.create_texture_from_hal::<wgpu::hal::dx12::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d {
                        width: desc.Width,
                        height: desc.Height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                },
            ))
        }
    }

    unsafe fn import_handle_vulkan(
        device: &wgpu::Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
        handle: HANDLE,
    ) -> Result<wgpu::Texture> {
        unsafe {
            let format =
                get_dxgi_yuv_format(desc.Format).unwrap(/* invariant by Self::new_vulkan */);

            let hal_device = device.as_hal::<wgpu::hal::vulkan::Api>().unwrap();
            let hal_texture = hal_device
                .texture_from_d3d11_shared_handle(
                    handle,
                    &wgpu::hal::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d {
                            width: desc.Width,
                            height: desc.Height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_DST,
                        memory_flags: wgpu::hal::MemoryFlags::empty(),
                        view_formats: vec![],
                    },
                )
                .map_err(|_| Error::TextureShare)?;

            Ok(device.create_texture_from_hal::<wgpu::hal::vulkan::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d {
                        width: desc.Width,
                        height: desc.Height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                },
            ))
        }
    }
}

fn acquire_ffmpeg_lock(
    lock: unsafe extern "C" fn(*mut c_void),
    unlock: unsafe extern "C" fn(*mut c_void),
    lock_ctx: *mut c_void,
) -> impl Drop {
    struct Guard(unsafe extern "C" fn(*mut c_void), *mut c_void);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                (self.0)(self.1);
            }
        }
    }
    unsafe {
        (lock)(lock_ctx);
    }
    Guard(unlock, lock_ctx)
}

pub struct D3D11VAFrameAdapter {
    d3d11_ctx: *mut AVD3D11VADeviceContext,
    d3d11_device: ManuallyDrop<D3D11::ID3D11Device>,
    imported_texture: Option<ImportedTexture>,
}

impl FrameAdapterBuilder for D3D11VAFrameAdapter {
    unsafe fn new(decoder: NonNull<ff::AVCodecContext>) -> Result<Self> {
        unsafe {
            let hwctx = decoder.as_ref().hw_device_ctx;

            let device_ctx = hwctx.as_ref().unwrap().data as *mut ff::AVHWDeviceContext;
            let d3d11_ctx = (*device_ctx).hwctx as *mut AVD3D11VADeviceContext;
            let d3d11_device: ManuallyDrop<D3D11::ID3D11Device> =
                ManuallyDrop::new(core::mem::transmute((*d3d11_ctx).device));

            Ok(D3D11VAFrameAdapter {
                d3d11_ctx,
                d3d11_device,
                imported_texture: None,
            })
        }
    }

    fn supports_format(format: ff::AVPixelFormat) -> bool {
        format == ff::AVPixelFormat::AV_PIX_FMT_D3D11
    }
}

impl FrameAdapter for D3D11VAFrameAdapter {
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        _instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<()> {
        unsafe {
            let frame = frame.as_ref();
            if frame.data[0].is_null() {
                return Err(Error::InvalidFrame);
            }

            if frame.format != ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                return Err(Error::UnsupportedPixelFormat);
            }

            let d3d11_texture: ManuallyDrop<D3D11::ID3D11Texture2D> =
                ManuallyDrop::new(core::mem::transmute(frame.data[0]));

            let mut desc = D3D11::D3D11_TEXTURE2D_DESC::default();
            d3d11_texture.GetDesc(&mut desc);

            // lock ffmpeg d3d11 context mutex
            let d3d11_lock = acquire_ffmpeg_lock(
                (*self.d3d11_ctx).lock,
                (*self.d3d11_ctx).unlock,
                (*self.d3d11_ctx).lock_ctx,
            );

            let imported_texture = if let Some(imported_texture) = &self.imported_texture {
                imported_texture
            } else {
                let color_space = frame.colorspace.into();
                let imported_texture = match adapter.get_info().backend {
                    wgpu::Backend::Vulkan => ImportedTexture::new_vulkan(
                        device,
                        pipeline_cache,
                        &self.d3d11_device,
                        &desc,
                        color_space,
                    )?,
                    wgpu::Backend::Dx12 => ImportedTexture::new_d3d12(
                        device,
                        pipeline_cache,
                        &self.d3d11_device,
                        &desc,
                        color_space,
                    )?,
                    _ => return Err(Error::UnsupportedBackend),
                };
                self.imported_texture.insert(imported_texture)
            };

            imported_texture
                    .shared_texture
                    .cast::<Dxgi::IDXGIKeyedMutex>()
                    .unwrap(/* texture created with SHARED_KEYEDMUTEX */)
                    .AcquireSync(0, u32::MAX)
                    .map_err(|_| Error::Unknown)?;

            self.d3d11_device
                .GetImmediateContext()
                .unwrap()
                .CopySubresourceRegion(
                    &imported_texture.shared_texture,
                    0,
                    0,
                    0,
                    0,
                    &*d3d11_texture,
                    frame.data[1] as u32,
                    None,
                );

            imported_texture
                    .shared_texture
                    .cast::<Dxgi::IDXGIKeyedMutex>()
                    .unwrap(/* texture created with SHARED_KEYEDMUTEX */)
                    .ReleaseSync(0)
                    .map_err(|_| Error::Unknown)?;

            // unlock ffmpeg mutex
            drop(d3d11_lock);

            Ok(())
        }
    }

    fn layout_identity(&self) -> Option<layout::FrameDescriptor<()>> {
        self.imported_texture
            .as_ref()
            .map(|texture| texture.identity)
    }

    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.imported_texture.as_ref().map(|texture| &texture.bg0)
    }

    fn name(&self) -> &'static str {
        "D3D11VA zero-copy"
    }
}
