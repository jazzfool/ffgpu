use super::FrameAdapter;
use crate::{
    Error,
    context::{layout, pipeline_cache::PipelineCache},
    decode::hw::FrameAdapterBuilder,
    error::Result,
};
use ash::vk;
use ffmpeg_next::sys as ff;
use std::ptr::NonNull;

struct VulkanDRMTexture {
    drm_frame: NonNull<ff::AVFrame>,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bg0: wgpu::BindGroup,
    identity: layout::FrameDescriptor<()>,
}

impl VulkanDRMTexture {
    unsafe fn new(
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        frame: NonNull<ff::AVFrame>,
    ) -> Self {
        unsafe {
            let frame = frame.as_ref();

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
                    width: frame.width as u32 / 2,
                    height: frame.height as u32 / 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let textures = layout::FrameDescriptor {
                planes: layout::PlaneLayout::PackedYUV420([y_texture.clone(), uv_texture.clone()]),
                depth: layout::Depth::D8,
            };

            let bg0 = pipeline_cache.bind_frame_textures(
                &layout::FrameDescriptor {
                    planes: layout::create_frame_texture_views(
                        &textures.planes,
                        &Default::default(),
                    ),
                    depth: layout::Depth::D8,
                },
                frame.colorspace.into(),
            );

            VulkanDRMTexture {
                drm_frame,
                y_texture,
                uv_texture,
                bg0,
                identity: textures.as_identity(),
            }
        }
    }

    unsafe fn import_frame(
        &mut self,
        instance: &wgpu::Instance,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        frame: NonNull<ff::AVFrame>,
    ) -> bool {
        unsafe {
            let frame_ref = frame.as_ref();
            let drm_frame_ref = self.drm_frame.as_mut();

            drm_frame_ref.format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as _;
            if ff::av_hwframe_map(
                self.drm_frame.as_ptr(),
                frame.as_ptr(),
                ff::AV_HWFRAME_MAP_READ as _,
            ) != 0
            {
                log::error!("failed to map VA-API frame to DRM frame, switching to software copy");
                return false;
            }

            let drm_desc = (drm_frame_ref.data[0] as *const ff::AVDRMFrameDescriptor)
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

            let r8_plane = &drm_desc.layers[0].planes[0];
            let gr88_plane = &drm_desc.layers[1].planes[0];

            let r8_object = &drm_desc.objects[r8_plane.object_index as usize];
            let gr88_object = &drm_desc.objects[gr88_plane.object_index as usize];

            let r8_buffer = vk_device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .push_next(
                            &mut vk::ExternalMemoryBufferCreateInfo::default()
                                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                        )
                        .size(r8_plane.pitch as u64 * frame_ref.height as u64)
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
                                .fd(libc::dup(r8_object.fd)),
                        )
                        .allocation_size(0x7FFFFFFF) // doesn't matter for imports
                        .memory_type_index(memory_type as _),
                    None,
                )
                .expect("vkAllocateMemory");

            vk_device
                .bind_buffer_memory(r8_buffer, r8_memory, r8_plane.offset as _)
                .expect("vkBindBufferMemory");

            let r8_buffer = device.create_buffer_from_hal::<wgpu::hal::vulkan::Api>(
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
                        .size(gr88_plane.pitch as u64 * frame_ref.height as u64 / 2)
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
                                .fd(libc::dup(gr88_object.fd)),
                        )
                        .allocation_size(0x7FFFFFFF) // doesn't matter for imports
                        .memory_type_index(memory_type as _),
                    None,
                )
                .expect("vkAllocateMemory");

            vk_device
                .bind_buffer_memory(gr88_buffer, gr88_memory, gr88_plane.offset as _)
                .expect("vkBindBufferMemory");

            let gr88_buffer = device.create_buffer_from_hal::<wgpu::hal::vulkan::Api>(
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
                        bytes_per_row: Some(r8_plane.pitch as _),
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
                        bytes_per_row: Some(gr88_plane.pitch as _),
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
                    width: frame_ref.width as u32 / 2,
                    height: frame_ref.height as u32 / 2,
                    depth_or_array_layers: 1,
                },
            );

            ff::av_frame_unref(self.drm_frame.as_ptr());

            true
        }
    }
}

impl Drop for VulkanDRMTexture {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.drm_frame.as_ptr());
        }
    }
}

pub struct VAAPIFrameAdapter {
    imported: Option<VulkanDRMTexture>,
}

impl FrameAdapterBuilder for VAAPIFrameAdapter {
    unsafe fn new(_decoder: NonNull<ff::AVCodecContext>) -> Result<Self> {
        Ok(VAAPIFrameAdapter { imported: None })
    }

    fn supports_format(format: ff::AVPixelFormat) -> bool {
        format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI
    }
}

impl FrameAdapter for VAAPIFrameAdapter {
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<()> {
        unsafe {
            let frame_ref = frame.as_ref();

            if frame_ref.format != ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                return Err(Error::UnsupportedPixelFormat);
            }

            let imported = if let Some(imported) = self.imported.as_mut() {
                imported
            } else {
                self.imported.insert(match adapter.get_info().backend {
                    wgpu::Backend::Vulkan => VulkanDRMTexture::new(device, pipeline_cache, frame),
                    _ => return Err(Error::UnsupportedBackend),
                })
            };

            imported.import_frame(instance, device, encoder, frame);

            Ok(())
        }
    }

    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.imported.as_ref().map(|imported| &imported.bg0)
    }

    fn layout_identity(&self) -> Option<layout::FrameDescriptor<()>> {
        self.imported.as_ref().map(|imported| imported.identity)
    }

    fn name(&self) -> &'static str {
        "VA-API Vulkan DMA"
    }
}
