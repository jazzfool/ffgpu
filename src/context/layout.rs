use ffmpeg_next::sys as ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaneLayout<T> {
    PackedYUV420([T; 2]),
    YUV420([T; 3]),
    YUV444([T; 3]),
    RGB(T),
}

pub struct PlaneDescriptor {
    pub width_div: u32,
    pub height_div: u32,
    pub channels: u32,
}

impl<T> PlaneLayout<T> {
    pub fn map<U>(&self, mut f: impl FnMut(&T, PlaneDescriptor) -> U) -> PlaneLayout<U> {
        let pdesc = |width_div, height_div, channels| PlaneDescriptor {
            width_div,
            height_div,
            channels,
        };

        match self {
            PlaneLayout::PackedYUV420([y, uv]) => {
                PlaneLayout::PackedYUV420([f(y, pdesc(1, 1, 1)), f(uv, pdesc(2, 2, 2))])
            }
            PlaneLayout::YUV420([y, u, v]) => PlaneLayout::YUV420([
                f(y, pdesc(1, 1, 1)),
                f(u, pdesc(2, 2, 1)),
                f(v, pdesc(2, 2, 1)),
            ]),
            PlaneLayout::YUV444([y, u, v]) => PlaneLayout::YUV444([
                f(y, pdesc(1, 1, 1)),
                f(u, pdesc(1, 1, 1)),
                f(v, pdesc(1, 1, 1)),
            ]),
            PlaneLayout::RGB(plane) => PlaneLayout::RGB(f(plane, pdesc(1, 1, 3))),
        }
    }

    pub fn as_identity(&self) -> PlaneLayout<()> {
        self.map(|_, _| ())
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
        ff::AVPixelFormat::AV_PIX_FMT_YUV444P => Some(FrameDescriptor {
            planes: PlaneLayout::YUV444([wgpu::TextureFormat::R8Unorm; 3]),
            depth: Depth::D8,
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
    planes: PlaneLayout<wgpu::TextureFormat>,
    width: u32,
    height: u32,
    usage: wgpu::TextureUsages,
) -> PlaneLayout<wgpu::Texture> {
    planes.map(|format, plane_desc| {
        create_texture(
            device,
            width / plane_desc.width_div,
            height / plane_desc.height_div,
            *format,
            usage,
        )
    })
}

pub fn create_frame_texture_views(
    textures: &PlaneLayout<wgpu::Texture>,
    desc: &wgpu::TextureViewDescriptor,
) -> PlaneLayout<wgpu::TextureView> {
    textures.map(|texture, _| texture.create_view(desc))
}
