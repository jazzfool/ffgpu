use crate::decode::{HardwareDecoder, av_version};
use ffmpeg_sys_next as ff;

pub struct VideoToolboxDecoder {}

impl HardwareDecoder for VideoToolboxDecoder {
    const DEVICE_TYPE: ff::AVHWDeviceType = ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
    const AVUTIL_VERSION: std::ops::RangeInclusive<u32> =
        av_version(55, 78, 100)..=av_version(60, 25, 100);

    unsafe fn new(hwctx: *mut ffmpeg_sys_next::AVBufferRef) -> Self {
        VideoToolboxDecoder {}
    }

    unsafe fn import_frame(
        &mut self,
        frame: std::ptr::NonNull<ffmpeg_sys_next::AVFrame>,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
    ) -> Option<&wgpu::BindGroup> {
        unsafe {
            let frame = frame.as_mut();
            if frame.data[3].is_null() {
                return None;
            }
        }
    }
}
