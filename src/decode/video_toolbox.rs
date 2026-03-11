use crate::{context::pipeline_cache::PipelineCache, decode::HardwareDecoder};
use ffmpeg_next::sys as ff;
use metal::foreign_types::ForeignType;
use objc2_core_video as cv;
use std::ptr::{NonNull, null_mut};

struct ImportedCVMetalTexture {
    texture_cache: NonNull<cv::CVMetalTextureCache>,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bg0: wgpu::BindGroup,
}

impl ImportedCVMetalTexture {
    unsafe fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        metal_device: &objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>,
        pixel_buffer: NonNull<cv::CVPixelBuffer>,
    ) -> Self {
        let texture_cache = null_mut();
        unsafe {
            cv::CVMetalTextureCache::create(
                None,
                None,
                metal_device,
                None,
                NonNull::from_ref(&texture_cache),
            );
        }
        let texture_cache = NonNull::new(texture_cache).expect("CVMetalTextureCacheCreate");

        let pixel_buffer = unsafe { pixel_buffer.as_ref() };
        let y_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 0);
        let y_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 0);
        let uv_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 1);
        let uv_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 1);

        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: y_width as _,
                height: y_height as _,
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
                width: uv_width as _,
                height: uv_height as _,
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

        ImportedCVMetalTexture {
            texture_cache,
            y_texture,
            uv_texture,
            bg0,
        }
    }

    unsafe fn import_cv_buffer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        mut pixel_buffer: NonNull<cv::CVPixelBuffer>,
    ) {
        let (y_cv_texture, uv_cv_texture) = unsafe {
            let pixel_buffer = pixel_buffer.as_mut();

            let y_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 0);
            let y_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 0);
            let y_texture = null_mut();
            cv::CVMetalTextureCache::create_texture_from_image(
                None,
                self.texture_cache.as_ref(),
                &*(&mut *pixel_buffer as *mut cv::CVImageBuffer), // CVPixelBuffer == CVImageBuffer
                None,
                objc2_metal::MTLPixelFormat::R8Unorm,
                y_width,
                y_height,
                0,
                NonNull::from_ref(&y_texture),
            );

            let uv_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 1);
            let uv_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 1);
            let uv_texture = null_mut();

            cv::CVMetalTextureCache::create_texture_from_image(
                None,
                self.texture_cache.as_ref(),
                &*(&mut *pixel_buffer as *mut cv::CVImageBuffer), // CVPixelBuffer == CVImageBuffer
                None,
                objc2_metal::MTLPixelFormat::RG8Unorm,
                uv_width,
                uv_height,
                1,
                NonNull::from_ref(&uv_texture),
            );

            (y_texture, uv_texture)
        };

        unsafe {
            // because wgpu 28 still uses metal instead of objc2-metal
            // it becomes *really* painful to convert types for use with objc2-*

            let y_mtl_texture =
                cv::CVMetalTextureGetTexture(&*y_cv_texture).expect("CVMetalTextureGetTexture");
            let uv_mtl_texture =
                cv::CVMetalTextureGetTexture(&*uv_cv_texture).expect("CVMetalTextureGetTexture");

            let y_dst_mtl_texture = self
                .y_texture
                .as_hal::<wgpu::hal::metal::Api>()
                .unwrap()
                .raw_handle()
                .clone();

            let uv_dst_mtl_texture = self
                .uv_texture
                .as_hal::<wgpu::hal::metal::Api>()
                .unwrap()
                .raw_handle()
                .clone();

            // there is already another command encoder in progress at this point (given by the user)
            // but we cannot use it since wgpu complains about mixing hal usage with wgpu usage! >:(
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            encoder.as_hal_mut::<wgpu::hal::metal::Api, _, _>(|encoder| {
                let y_mtl_texture =
                    metal::Texture::from_ptr(objc2::rc::Retained::as_ptr(&y_mtl_texture) as *mut _);
                let uv_mtl_texture = metal::Texture::from_ptr(objc2::rc::Retained::as_ptr(
                    &uv_mtl_texture,
                ) as *mut _);

                let command_buffer = encoder.unwrap().raw_command_buffer().unwrap();
                let blit_encoder = command_buffer.new_blit_command_encoder();
                blit_encoder.copy_from_texture(
                    &y_mtl_texture,
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                    metal::MTLSize {
                        width: self.y_texture.width() as _,
                        height: self.y_texture.height() as _,
                        depth: 1,
                    },
                    &y_dst_mtl_texture,
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                );

                blit_encoder.copy_from_texture(
                    &uv_mtl_texture,
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                    metal::MTLSize {
                        width: uv_mtl_texture.width() as _,
                        height: uv_mtl_texture.height() as _,
                        depth: 1,
                    },
                    &uv_dst_mtl_texture,
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                );

                blit_encoder.end_encoding();
            });
            queue.submit(Some(encoder.finish()));
        }
    }
}

struct CopiedTexture {
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bg0: wgpu::BindGroup,
}

impl CopiedTexture {
    fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        pixel_buffer: NonNull<cv::CVPixelBuffer>,
    ) -> Self {
        let pixel_buffer = unsafe { pixel_buffer.as_ref() };
        let y_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 0);
        let y_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 0);
        let uv_width = cv::CVPixelBufferGetWidthOfPlane(pixel_buffer, 1);
        let uv_height = cv::CVPixelBufferGetHeightOfPlane(pixel_buffer, 1);

        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: y_width as _,
                height: y_height as _,
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
                width: uv_width as _,
                height: uv_height as _,
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
            y_texture,
            uv_texture,
            bg0,
        }
    }

    unsafe fn import_cv_buffer(
        &self,
        queue: &wgpu::Queue,
        pixel_buffer: NonNull<cv::CVPixelBuffer>,
    ) {
        let pixel_buffer = unsafe { pixel_buffer.as_ref() };
        unsafe {
            cv::CVPixelBufferLockBaseAddress(pixel_buffer, cv::CVPixelBufferLockFlags::ReadOnly)
        };
        let y_data = cv::CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0);
        let uv_data = cv::CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1);
        let y_bytes_per_row = cv::CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0);
        let uv_bytes_per_row = cv::CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1);

        let y_data = unsafe {
            core::slice::from_raw_parts(
                y_data as *const u8,
                y_bytes_per_row * self.y_texture.height() as usize,
            )
        };
        let uv_data = unsafe {
            core::slice::from_raw_parts(
                uv_data as *const u8,
                uv_bytes_per_row * self.uv_texture.height() as usize,
            )
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            y_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(y_bytes_per_row as _),
                rows_per_image: Some(self.y_texture.height()),
            },
            wgpu::Extent3d {
                width: self.y_texture.width(),
                height: self.y_texture.height(),
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
            uv_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(uv_bytes_per_row as _),
                rows_per_image: Some(self.uv_texture.height()),
            },
            wgpu::Extent3d {
                width: self.uv_texture.width(),
                height: self.uv_texture.height(),
                depth_or_array_layers: 1,
            },
        );

        unsafe {
            cv::CVPixelBufferUnlockBaseAddress(pixel_buffer, cv::CVPixelBufferLockFlags::ReadOnly)
        };
    }
}

enum ImportedTexture {
    CVMetalTexture(ImportedCVMetalTexture),
    PlanarCopy(CopiedTexture),
}

pub struct VideoToolboxHardwareDecoder {
    imported_texture: Option<ImportedTexture>,
}

impl HardwareDecoder for VideoToolboxHardwareDecoder {
    const DEVICE_TYPE: ff::AVHWDeviceType = ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;

    unsafe fn new(_hwctx: NonNull<ff::AVBufferRef>) -> Self {
        VideoToolboxHardwareDecoder {
            imported_texture: None,
        }
    }

    unsafe fn import_frame(
        &mut self,
        mut frame: NonNull<ff::AVFrame>,
        _instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        layout: &wgpu::BindGroupLayout,
    ) -> Option<&wgpu::BindGroup> {
        unsafe {
            let frame = frame.as_mut();
            if frame.data[3].is_null() {
                return None;
            }

            assert_eq!(
                frame.format,
                ff::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32,
                "unexpected frame AVPixelFormat, expected VideoToolbox frame"
            );

            let pixel_buffer = NonNull::new_unchecked(frame.data[3] as *mut cv::CVPixelBuffer);

            let imported_texture =
                self.imported_texture
                    .get_or_insert_with(|| match adapter.get_info().backend {
                        wgpu::Backend::Metal => {
                            let hal_device = device.as_hal::<wgpu::hal::metal::Api>().unwrap();
                            let metal_device =
                                hal_device.raw_device().as_ptr() as *mut objc2::runtime::AnyObject;
                            let metal_device = metal_device
                                .cast::<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>>(
                                )
                                .as_ref()
                                .unwrap();
                            ImportedTexture::CVMetalTexture(ImportedCVMetalTexture::new(
                                device,
                                layout,
                                metal_device,
                                pixel_buffer,
                            ))
                        }
                        backend => {
                            log::warn!(
                                "unsupported zero-copy WGPU backend {} (must be Metal)",
                                backend
                            );
                            log::warn!("using CPU frame copies");
                            ImportedTexture::PlanarCopy(CopiedTexture::new(
                                device,
                                layout,
                                pixel_buffer,
                            ))
                        }
                    });

            let bg0 = match imported_texture {
                ImportedTexture::CVMetalTexture(imported_texture) => {
                    imported_texture.import_cv_buffer(device, queue, pixel_buffer);
                    &imported_texture.bg0
                }
                ImportedTexture::PlanarCopy(imported_texture) => {
                    imported_texture.import_cv_buffer(queue, pixel_buffer);
                    &imported_texture.bg0
                }
            };

            Some(bg0)
        }
    }
}
