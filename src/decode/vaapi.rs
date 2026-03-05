// ffmpeg should give us a DRM PRIME descriptor
// import it into a VkBuffer, wrap as wgpu::Buffer
// copy buffer to wgpu::Texture

use crate::context::pipeline_cache::PipelineCache;

use super::{HardwareDecoder, av_version};
use ash::vk;
use ffmpeg_sys_next as ff;
use std::ptr::{NonNull, null_mut};

struct ImportedVulkanFrame {
    drm_frames: NonNull<ff::AVBufferRef>,
    drm_frame: NonNull<ff::AVFrame>,
    image: vk::Image,
    bg0: wgpu::BindGroup,
}

impl ImportedVulkanFrame {
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

            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: frame.width as _,
                    height: frame.height as _,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let image = texture
                .as_hal::<wgpu::hal::vulkan::Api>()
                .unwrap()
                .raw_handle();

            let bg0 = PipelineCache::create_nv12_bind_group(device, &texture, layout);

            ImportedVulkanFrame {
                drm_frames,
                drm_frame,
                image,
                bg0,
            }
        }
    }

    unsafe fn import_frame(&self, device: &wgpu::Device, frame: NonNull<ff::AVFrame>) {
        unsafe {
            // TODO: handle gracefully (i.e., fallback to CPU copy frames)
            assert_eq!(
                ff::av_hwframe_map(
                    self.drm_frame.as_ptr(),
                    frame.as_ptr(),
                    ff::AV_HWFRAME_MAP_READ as _,
                ),
                0,
                "failed to map VA-API frame to DRM frame"
            );

            let drm_frame = self.drm_frame.as_ref();
            let drm_desc = (drm_frame.data[0] as *const ff::AVDRMFrameDescriptor)
                .as_ref()
                .unwrap();

            assert_eq!(
                drm_desc.nb_layers, 2,
                "expected 2 layers in AVDRMFrameDescriptor"
            );
            assert_eq!(
                drm_desc.layers[0].format,
                538982482, // DRM_FORMAT_R8 (see linux/drm/drm_fourcc.h)
                "expected layer 0 DRM_FORMAT_R8"
            );
            assert_eq!(
                drm_desc.layers[1].format,
                943215175, // DRM_FORMAT_GR88
                "expected layer 0 DRM_FORMAT_GR88"
            );

            vk::ExternalMemoryBufferCreateInfo {
                handle_types: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                ..Default::default()
            };
        }
    }
}

impl Drop for ImportedVulkanFrame {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.drm_frame.as_ptr());
            ff::av_buffer_unref(&mut self.drm_frames.as_ptr());
        }
    }
}

struct VAAPIHardwareDecoder {
    drm_hwctx: NonNull<ff::AVBufferRef>,
    imported: Option<ImportedVulkanFrame>,
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
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
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
                    wgpu::Backend::Vulkan => {
                        ImportedVulkanFrame::new(device, layout, self.drm_hwctx, frame)
                    }
                    _ => panic!(),
                });

            imported.import_frame(frame);

            Some(&imported.bg0)
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
