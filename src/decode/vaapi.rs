use super::{HardwareDecoder, av_version};
use crate::context::pipeline_cache::PipelineCache;
use ash::vk;
use ffmpeg_sys_next as ff;
use std::ptr::{NonNull, null_mut};

struct VulkanDRMTexture {
    drm_frames: NonNull<ff::AVBufferRef>,
    drm_frame: NonNull<ff::AVFrame>,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bg0: wgpu::BindGroup,
}

impl VulkanDRMTexture {
    unsafe fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        drm_hwctx: NonNull<ff::AVBufferRef>,
        frame: NonNull<ff::AVFrame>,
    ) -> Self {
        unsafe {
            let frame = frame.as_ref();

            let drm_frames = ff::av_hwframe_ctx_alloc(drm_hwctx.as_ptr());
            let drm_frames = NonNull::new(drm_frames).expect("av_hwframe_ctx_alloc DRM frames");

            let frames_ctx = (drm_frames.as_ref().data as *mut ff::AVHWFramesContext)
                .as_mut()
                .expect("DRM frames data AVHwFramesContext");
            frames_ctx.format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            frames_ctx.sw_format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
            frames_ctx.width = frame.width;
            frames_ctx.height = frame.height;

            assert_eq!(
                ff::av_hwframe_ctx_init(drm_frames.as_ptr()),
                0,
                "av_hwframe_ctx_init"
            );

            let drm_frame = NonNull::new(ff::av_frame_alloc()).expect("av_frame_alloc");

            let y_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: frame.width as _,
                    height: frame.height as _,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: frame.width as _,
                    height: frame.height as _,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let bg0 =
                PipelineCache::create_planar_bind_group(device, &y_texture, &uv_texture, layout);

            VulkanDRMTexture {
                drm_frames,
                drm_frame,
                y_texture,
                uv_texture,
                bg0,
            }
        }
    }

    unsafe fn import_frame(
        &self,
        instance: &wgpu::Instance,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        frame: NonNull<ff::AVFrame>,
    ) -> bool {
        unsafe {
            let frame_ref = frame.as_ref();

            if ff::av_hwframe_map(
                self.drm_frame.as_ptr(),
                frame.as_ptr(),
                ff::AV_HWFRAME_MAP_READ as _,
            ) != 0
            {
                log::error!("failed to map VA-API frame to DRM frame, switching to software copy");
                return false;
            }

            let drm_frame = self.drm_frame.as_ref();
            let drm_desc = (drm_frame.data[0] as *const ff::AVDRMFrameDescriptor)
                .as_ref()
                .unwrap();

            // TODO: handle more drm formats/planes?
            if drm_desc.nb_layers != 2
                || drm_desc.layers[0].format != (538982482/* DRM_FORMAT_R8 */)
                || drm_desc.layers[1].format != (943215175/* DRM_FORMAT_GR88 */)
            {
                log::error!("unsupported AVDRMFrameDescriptor layout (expected R8+GR88 planes)");
                return false;
            }

            let vk_device = device
                .as_hal::<wgpu::hal::vulkan::Api>()
                .unwrap()
                .raw_device()
                .clone();

            let vk_physical_device = device
                .as_hal::<wgpu::hal::vulkan::Api>()
                .unwrap()
                .raw_physical_device();

            let memory_properties = instance
                .as_hal::<wgpu::hal::vulkan::Api>()
                .unwrap()
                .shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(vk_physical_device);

            let memory_type = memory_properties
                .memory_types_as_slice()
                .iter()
                .position(|memory_type| {
                    memory_type
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .unwrap_or(0);

            let r8_object = &drm_desc.objects[drm_desc.layers[0].planes[0].object_index as usize];
            let gr88_object = &drm_desc.objects[drm_desc.layers[1].planes[0].object_index as usize];

            let r8_buffer = vk_device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .push_next(
                            &mut vk::ExternalMemoryBufferCreateInfo::default()
                                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                        )
                        .size(r8_object.size as _)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("vkCreateBuffer");

            let r8_memory = vk_device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .push_next(
                            &mut vk::ImportMemoryFdInfoKHR::default()
                                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                                .fd(nix::unistd::dup(r8_object.fd)),
                        )
                        .allocation_size(0x7FFFFFFF) // doesn't matter for imports
                        .memory_type_index(memory_type as _),
                    None,
                )
                .expect("vkAllocateMemory");

            vk_device
                .bind_buffer_memory(r8_buffer, r8_memory, 0)
                .expect("vkBindBufferMemory");

            let r8_buffer = device.create_buffer_from_hal(
                wgpu::hal::vulkan::Buffer::from_raw_managed(
                    r8_buffer,
                    r8_memory,
                    0,
                    r8_object.size as _,
                ),
                &wgpu::BufferDescriptor {
                    label: None,
                    size: r8_object.size as _,
                    usage: wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                },
            );

            let gr88_buffer = vk_device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .push_next(
                            &mut vk::ExternalMemoryBufferCreateInfo::default()
                                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                        )
                        .size(gr88_object.size as _)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .expect("vkCreateBuffer");

            let gr88_memory = vk_device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .push_next(
                            &mut vk::ImportMemoryFdInfoKHR::default()
                                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                                .fd(nix::unistd::dup(gr88_object.fd)),
                        )
                        .allocation_size(0x7FFFFFFF) // doesn't matter for imports
                        .memory_type_index(memory_type as _),
                    None,
                )
                .expect("vkAllocateMemory");

            vk_device
                .bind_buffer_memory(gr88_buffer, gr88_memory, 0)
                .expect("vkBindBufferMemory");

            let gr88_buffer = device.create_buffer_from_hal(
                wgpu::hal::vulkan::Buffer::from_raw_managed(
                    gr88_buffer,
                    gr88_memory,
                    0,
                    gr88_object.size as _,
                ),
                &wgpu::BufferDescriptor {
                    label: None,
                    size: gr88_object.size as _,
                    usage: wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                },
            );

            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &r8_buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(drm_desc.layers[0].planes[0].pitch as _),
                        rows_per_image: None,
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &self.y_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: frame_ref.width as _,
                    height: frame_ref.height as _,
                    depth_or_array_layers: 1,
                },
            );

            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &gr88_buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(drm_desc.layers[1].planes[0].pitch as _),
                        rows_per_image: None,
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &self.uv_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: frame_ref.width as _ / 2,
                    height: frame_ref.height as _ / 2,
                    depth_or_array_layers: 1,
                },
            );

            true
        }
    }
}

impl Drop for VulkanDRMTexture {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.drm_frame.as_ptr());
            ff::av_buffer_unref(&mut self.drm_frames.as_ptr());
        }
    }
}

struct CopiedTexture {
    nv12_frame: NonNull<ff::AVFrame>,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bg0: wgpu::BindGroup,
}

impl CopiedTexture {
    fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        frame: NonNull<ff::AVFrame>,
    ) -> Self {
        let frame = unsafe { frame.as_ref() };

        let nv12_frame = unsafe { NonNull::new(ff::av_frame_alloc()).expect("av_frame_alloc") };

        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: frame.width as _,
                height: frame.height as _,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: frame.width as _,
                height: frame.height as _,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let bg0 = PipelineCache::create_planar_bind_group(device, &y_texture, &uv_texture, layout);

        CopiedTexture {
            nv12_frame,
            y_texture,
            uv_texture,
            bg0,
        }
    }

    unsafe fn import_frame(&mut self, queue: &wgpu::Queue, frame: NonNull<ff::AVFrame>) {
        unsafe {
            let frame_ref = frame.as_ref();
            let nv12_frame_ref = self.nv12_frame.as_mut();

            nv12_frame_ref.format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as _;
            ff::av_hwframe_map(self.nv12_frame.as_ptr(), frame.as_ptr(), 0);

            let data = core::slice::from_raw_parts(
                nv12_frame_ref.data[0],
                (frame_ref.width * frame_ref.height * 3 / 2) as _,
            );

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.y_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data[..(frame_ref.width * frame_ref.height) as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame_ref.width as _),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: frame_ref.width as _,
                    height: frame_ref.height as _,
                    depth_or_array_layers: 1,
                },
            );

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.uv_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data[(frame_ref.width * frame_ref.height) as usize..],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame_ref.width as _),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: frame_ref.width as u32 / 2,
                    height: frame_ref.height as u32 / 2,
                    depth_or_array_layers: 1,
                },
            );

            ff::av_frame_unref(self.nv12_frame.as_ptr());
        }
    }
}

impl Drop for CopiedTexture {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.nv12_frame.as_ptr());
        }
    }
}

enum ImportedTexture {
    VulkanDRM(VulkanDRMTexture),
    PlanarCopy(CopiedTexture),
}

struct VAAPIHardwareDecoder {
    drm_hwctx: NonNull<ff::AVBufferRef>,
    imported: Option<ImportedTexture>,
}

impl HardwareDecoder for VAAPIHardwareDecoder {
    const DEVICE_TYPE: ff::AVHWDeviceType = ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
    const AVUTIL_VERSION: std::ops::RangeInclusive<u32> =
        av_version(55, 78, 100)..=av_version(60, 25, 100);

    unsafe fn new(hwctx: *mut ff::AVBufferRef) -> Self {
        unsafe {
            let mut drm_hwctx = null_mut();
            ff::av_hwdevice_ctx_create_derived(
                &mut drm_hwctx,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                hwctx,
                0,
            );
            let drm_hwctx = NonNull::new(drm_hwctx)
                .expect("av_hwdevice_ctx_create_derived AV_HWDEVICE_TYPE_DRM");

            VAAPIHardwareDecoder {
                drm_hwctx,
                imported: None,
            }
        }
    }

    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        layout: &wgpu::BindGroupLayout,
    ) -> Option<&wgpu::BindGroup> {
        unsafe {
            let frame_ref = frame.as_ref();

            assert_eq!(
                frame_ref.format,
                ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32,
                "unexpected frame AVPixelFormat, expected VA-API frame"
            );

            let imported = self
                .imported
                .get_or_insert_with(|| match adapter.get_info().backend {
                    wgpu::Backend::Vulkan => ImportedTexture::VulkanDRM(VulkanDRMTexture::new(
                        device,
                        layout,
                        self.drm_hwctx,
                        frame,
                    )),
                    _ => {
                        log::warn!("unsupported zero-copy WGPU backend (must be Vulkan)");
                        log::warn!("using CPU frame copies");
                        ImportedTexture::PlanarCopy(CopiedTexture::new(device, layout, frame))
                    }
                });

            let force_planar_copy = match imported {
                ImportedTexture::VulkanDRM(imported) => {
                    !imported.import_frame(instance, device, encoder, frame)
                }
                ImportedTexture::PlanarCopy(imported) => {
                    imported.import_frame(queue, frame);
                    false
                }
            };

            if force_planar_copy {
                self.imported = Some(ImportedTexture::PlanarCopy(CopiedTexture::new(
                    device, layout, frame,
                )));
                return None;
            }

            self.imported.as_ref().map(|imported| match imported {
                ImportedTexture::VulkanDRM(imported) => &imported.bg0,
                ImportedTexture::PlanarCopy(imported) => &imported.bg0,
            })
        }
    }
}

impl Drop for VAAPIHardwareDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_buffer_unref(&mut self.drm_hwctx.as_ptr());
        }
    }
}
