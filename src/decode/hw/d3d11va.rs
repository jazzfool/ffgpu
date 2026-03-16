use super::HardwareDecoder;
use crate::{
    context::pipeline_cache::PipelineCache,
    error::{Error, Result},
};
use ffmpeg_next::sys as ff;
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

enum TextureDestination {
    ExternalYUV,
    PlanarCopy {
        y_texture: wgpu::Texture,
        uv_texture: wgpu::Texture,
    },
}

fn get_dxgi_yuv_format(format: Dxgi::Common::DXGI_FORMAT) -> Option<wgpu::TextureFormat> {
    match format {
        Dxgi::Common::DXGI_FORMAT_NV12 => Some(wgpu::TextureFormat::NV12),
        Dxgi::Common::DXGI_FORMAT_P010 => Some(wgpu::TextureFormat::P010),
        _ => None,
    }
}

fn get_dxgi_planar_format(format: Dxgi::Common::DXGI_FORMAT) -> Option<[wgpu::TextureFormat; 2]> {
    match format {
        Dxgi::Common::DXGI_FORMAT_NV12 => {
            Some([wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm])
        }
        Dxgi::Common::DXGI_FORMAT_P010 => Some([
            wgpu::TextureFormat::R16Unorm,
            wgpu::TextureFormat::Rg16Unorm,
        ]),
        _ => None,
    }
}

struct ImportedTexture {
    // TODO(jazzfool): FFmpeg recently added the ability to specify SHARED misc flag (not yet released as of 02/26)
    // by passing "SHARED" in an AVDictionary to hwcontext_d3d11va
    // which means we could skip copying to another D3D11 with the SHARED flag and directly export the FFmpeg-created texture
    shared_texture: D3D11::ID3D11Texture2D,
    destination: TextureDestination,
    bg0: wgpu::BindGroup,
}

impl ImportedTexture {
    unsafe fn new_d3d12(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        if get_dxgi_yuv_format(desc.Format).is_none() {
            return Err(Error::UnsupportedPixelFormat);
        }

        unsafe {
            let (shared_texture, desc, handle) =
                ImportedTexture::create_shared_texture(d3d11_device, desc)?;
            let texture = ImportedTexture::import_handle_d3d12(device, &desc, handle)?;
            let bg0 = PipelineCache::create_yuv_bind_group(device, &texture, layout);
            Ok(ImportedTexture {
                shared_texture,
                destination: TextureDestination::ExternalYUV,
                bg0,
            })
        }
    }

    unsafe fn new_vulkan(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        if get_dxgi_yuv_format(desc.Format).is_none() {
            return Err(Error::UnsupportedPixelFormat);
        }

        unsafe {
            let (shared_texture, desc, handle) =
                ImportedTexture::create_shared_texture(d3d11_device, desc)?;
            let texture = ImportedTexture::import_handle_vulkan(device, &desc, handle)?;
            let bg0 = PipelineCache::create_yuv_bind_group(device, &texture, layout);
            Ok(ImportedTexture {
                shared_texture,
                destination: TextureDestination::ExternalYUV,
                bg0,
            })
        }
    }

    unsafe fn new_cpu(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        if get_dxgi_planar_format(desc.Format).is_none() {
            return Err(Error::UnsupportedPixelFormat);
        }

        unsafe {
            let (shared_texture, desc) =
                ImportedTexture::create_mapped_texture(d3d11_device, desc)?;
            let (y_texture, uv_texture) = ImportedTexture::create_planar_textures(device, &desc);
            let bg0 =
                PipelineCache::create_planar_bind_group(device, &y_texture, &uv_texture, layout);
            Ok(ImportedTexture {
                shared_texture,
                destination: TextureDestination::PlanarCopy {
                    y_texture,
                    uv_texture,
                },
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

    unsafe fn create_mapped_texture(
        d3d11_device: &D3D11::ID3D11Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> Result<(D3D11::ID3D11Texture2D, D3D11::D3D11_TEXTURE2D_DESC)> {
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
                Usage: D3D11::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11::D3D11_CPU_ACCESS_READ.0 as _,
                MiscFlags: 0,
            };
            let mut texture = None;
            d3d11_device
                .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                .map_err(|_| Error::TextureShare)?;
            let Some(texture) = texture else { panic!() };

            Ok((texture, texture_desc))
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

    unsafe fn create_planar_textures(
        device: &wgpu::Device,
        desc: &D3D11::D3D11_TEXTURE2D_DESC,
    ) -> (wgpu::Texture, wgpu::Texture) {
        let [y_format, uv_format] =
            get_dxgi_planar_format(desc.Format).unwrap(/* invariant by Self::new_cpu */);

        let texture_y = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: desc.Width,
                height: desc.Height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: y_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let texture_uv = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: desc.Width / 2,
                height: desc.Height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: uv_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        (texture_y, texture_uv)
    }

    unsafe fn on_copy(&self, queue: &wgpu::Queue, d3d11_device: &D3D11::ID3D11Device) {
        match &self.destination {
            /* do nothing - the changes to the shared D3D11 texture will be visible in the wgpu texture */
            TextureDestination::ExternalYUV => {}
            TextureDestination::PlanarCopy {
                y_texture,
                uv_texture,
            } => {
                let mut mapped = D3D11::D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    d3d11_device
                        .GetImmediateContext()
                        .unwrap()
                        .Map(
                            &self.shared_texture,
                            0,
                            D3D11::D3D11_MAP_READ,
                            0,
                            Some(&mut mapped),
                        )
                        .unwrap()
                };

                let data = unsafe {
                    core::slice::from_raw_parts(
                        mapped.pData as *const u8,
                        mapped.RowPitch as usize * (y_texture.height() as usize * 3 / 2),
                    )
                };

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: y_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data[..(mapped.RowPitch * y_texture.height()) as usize],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(mapped.RowPitch),
                        rows_per_image: Some(y_texture.height()),
                    },
                    wgpu::Extent3d {
                        width: y_texture.width(),
                        height: y_texture.height(),
                        depth_or_array_layers: 1,
                    },
                );

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: uv_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data[(mapped.RowPitch * y_texture.height()) as usize..],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(mapped.RowPitch),
                        rows_per_image: Some(uv_texture.height()),
                    },
                    wgpu::Extent3d {
                        width: uv_texture.width(),
                        height: uv_texture.height(),
                        depth_or_array_layers: 1,
                    },
                );

                unsafe {
                    d3d11_device
                        .GetImmediateContext()
                        .unwrap()
                        .Unmap(&self.shared_texture, 0)
                };
            }
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

pub struct D3D11VAHardwareDecoder {
    d3d11_ctx: *mut AVD3D11VADeviceContext,
    d3d11_device: ManuallyDrop<D3D11::ID3D11Device>,
    imported_texture: Option<ImportedTexture>,
}

impl HardwareDecoder for D3D11VAHardwareDecoder {
    const DEVICE_TYPE: ff::AVHWDeviceType = ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;

    unsafe fn new(hwctx: NonNull<ff::AVBufferRef>) -> Result<Self> {
        unsafe {
            let device_ctx = hwctx.as_ref().data as *mut ff::AVHWDeviceContext;
            let d3d11_ctx = (*device_ctx).hwctx as *mut AVD3D11VADeviceContext;
            let d3d11_device: ManuallyDrop<D3D11::ID3D11Device> =
                ManuallyDrop::new(core::mem::transmute((*d3d11_ctx).device));

            Ok(D3D11VAHardwareDecoder {
                d3d11_ctx,
                d3d11_device,
                imported_texture: None,
            })
        }
    }

    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        _instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        layout: &wgpu::BindGroupLayout,
    ) -> Result<()> {
        unsafe {
            let frame = frame.as_ref();
            if frame.data[0].is_null() {
                return Err(Error::InvalidFrame);
            }

            assert_eq!(
                frame.format,
                ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32,
                "unexpected frame AVPixelFormat, expected D3D11 frame"
            );

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
                let imported_texture = match adapter.get_info().backend {
                    wgpu::Backend::Vulkan => {
                        ImportedTexture::new_vulkan(device, layout, &self.d3d11_device, &desc)?
                    }
                    wgpu::Backend::Dx12 => {
                        ImportedTexture::new_d3d12(device, layout, &self.d3d11_device, &desc)?
                    }
                    _ => {
                        log::warn!("unsupported zero-copy WGPU backend (must be Vulkan or DX12)");
                        log::warn!("using CPU frame copies");
                        ImportedTexture::new_cpu(device, layout, &self.d3d11_device, &desc)?
                    }
                };
                self.imported_texture.insert(imported_texture)
            };

            if let TextureDestination::ExternalYUV = &imported_texture.destination {
                imported_texture
                    .shared_texture
                    .cast::<Dxgi::IDXGIKeyedMutex>()
                    .unwrap(/* texture created with SHARED_KEYEDMUTEX */)
                    .AcquireSync(0, u32::MAX)
                    .map_err(|_| Error::Unknown)?;
            }
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
            if let TextureDestination::ExternalYUV = imported_texture.destination {
                imported_texture
                    .shared_texture
                    .cast::<Dxgi::IDXGIKeyedMutex>()
                    .unwrap(/* texture created with SHARED_KEYEDMUTEX */)
                    .ReleaseSync(0)
                    .map_err(|_| Error::Unknown)?;
            }

            imported_texture.on_copy(queue, &self.d3d11_device);

            // unlock ffmpeg mutex
            drop(d3d11_lock);

            Ok(())
        }
    }

    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.imported_texture.as_ref().map(|texture| &texture.bg0)
    }

    fn name(&self) -> &'static str {
        match &self
            .imported_texture
            .as_ref()
            .map(|imported| &imported.destination)
        {
            Some(TextureDestination::ExternalYUV) => "D3D11VA zero-copy",
            Some(TextureDestination::PlanarCopy { .. }) => "D3D11VA software copy",
            None => "D3D11VA",
        }
    }
}
