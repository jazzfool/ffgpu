use ffmpeg_next::sys as ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaneLayout<T> {
    PackedYUV420([T; 2]),
    YUV420([T; 3]),
    RGB(T),
}

impl<T> PlaneLayout<T> {
    pub fn as_identity(&self) -> PlaneLayout<()> {
        match self {
            PlaneLayout::PackedYUV420(_) => PlaneLayout::PackedYUV420([(); 2]),
            PlaneLayout::YUV420(_) => PlaneLayout::YUV420([(); 3]),
            PlaneLayout::RGB(_) => PlaneLayout::RGB(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Depth {
    D8,
    D10,
    D12,
    D16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameDescriptor<T> {
    pub planes: PlaneLayout<T>,
    pub depth: Depth,
}

impl<T> FrameDescriptor<T> {
    pub fn as_identity(&self) -> FrameDescriptor<()> {
        FrameDescriptor {
            planes: self.planes.as_identity(),
            depth: self.depth,
        }
    }
}

pub fn av_pixel_texture_format(
    format: ff::AVPixelFormat,
) -> Option<FrameDescriptor<wgpu::TextureFormat>> {
    match format {
        ff::AVPixelFormat::AV_PIX_FMT_NV12 => Some(FrameDescriptor {
            planes: PlaneLayout::PackedYUV420([
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureFormat::Rg8Unorm,
            ]),
            depth: Depth::D8,
        }),
        ff::AVPixelFormat::AV_PIX_FMT_YUV420P => Some(FrameDescriptor {
            planes: PlaneLayout::YUV420([wgpu::TextureFormat::R8Unorm; 3]),
            depth: Depth::D8,
        }),
        ff::AVPixelFormat::AV_PIX_FMT_YUV420P12LE => Some(FrameDescriptor {
            planes: PlaneLayout::YUV420([wgpu::TextureFormat::R16Unorm; 3]),
            depth: Depth::D12,
        }),
        ff::AVPixelFormat::AV_PIX_FMT_P010LE => Some(FrameDescriptor {
            planes: PlaneLayout::PackedYUV420([
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            ]),
            depth: Depth::D16, // don't apply scaling in shader
        }),
        _ => None,
    }
}

fn create_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

pub fn create_frame_textures(
    device: &wgpu::Device,
    desc: FrameDescriptor<wgpu::TextureFormat>,
    width: u32,
    height: u32,
    usage: wgpu::TextureUsages,
) -> Option<FrameDescriptor<wgpu::Texture>> {
    match (desc.planes, desc.depth) {
        (PlaneLayout::PackedYUV420([y, uv]), _) => Some(FrameDescriptor {
            planes: PlaneLayout::PackedYUV420([
                create_texture(device, width, height, y, usage),
                create_texture(device, width / 2, height / 2, uv, usage),
            ]),
            depth: desc.depth,
        }),
        (PlaneLayout::YUV420([y, u, v]), _) => Some(FrameDescriptor {
            planes: PlaneLayout::YUV420([
                create_texture(device, width, height, y, usage),
                create_texture(device, width, height, u, usage),
                create_texture(device, width, height, v, usage),
            ]),
            depth: desc.depth,
        }),
        _ => None,
    }
}

pub fn create_frame_texture_views(
    textures: &PlaneLayout<wgpu::Texture>,
    desc: &wgpu::TextureViewDescriptor,
) -> PlaneLayout<wgpu::TextureView> {
    match textures {
        PlaneLayout::PackedYUV420([y, uv]) => {
            PlaneLayout::PackedYUV420([y.create_view(desc), uv.create_view(desc)])
        }
        PlaneLayout::YUV420([y, u, v]) => PlaneLayout::YUV420([
            y.create_view(desc),
            u.create_view(desc),
            v.create_view(desc),
        ]),
        PlaneLayout::RGB(rgb) => PlaneLayout::RGB(rgb.create_view(desc)),
    }
}
