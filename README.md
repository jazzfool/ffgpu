# ffgpu

A small experiment to bridge libavcodec/FFmpeg to WGPU for zero (CPU) copy GPU-accelerated video decoding.

The primary goal of this library is to bring simple and fast video playback to WGPU-based applications.

Supported platforms:

| Hardware decoder         | **Vulkan** | **DX12** | **Metal** | **OpenGL** |
|--------------------------|------------|----------|-----------|------------|
| **Windows (D3D11VA)**    | Yes        | Yes      | N/A       | CPU        |
| **MacOS (VideoToolbox)** | CPU        | N/A      | Yes       | CPU        |
| **Linux (VA-API DRM)**   | Yes        | N/A      | N/A       | CPU        |

I have no plans to support zero-copy on the OpenGL backend on any platform (but PRs for this are welcome of course).
The same goes for web and mobile platforms (Android and iOS).

This library is very incomplete and the following features are missing/WIP (roughly in order of priority):
- Wider YUV format support + 10-bit support
- Network streams
- Stream query and selection
- Subtitle decoding (including from a separate file)
- Software decoding fallback
- Fast thumbnailing; directly downsampled to an RGB texture atlas array from the YUV texture.

Full zero-copy (including GPU copies) is currently unachievable due to upstream limitations. To name a couple:
- ffmpeg <=8.0 does not expose the ability to modify the texture usage flags on the D3D11VA decoder (in this case, `SHARED`). This has already been fixed in ffmpeg trunk but is unreleased.
- wgpu does not have any way to (and does not by opportunity) request `VK_EXT_image_drm_format_modifier`. As such, dma buffers given by FFmpeg must be imported and copied to a `VkImage`. Mesa drivers also do not support `VK_EXT_image_drm_format_modifier` on certain graphics cards.

## License

Licensed under either

- [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0)
- [MIT](http://opensource.org/licenses/MIT)

at your option.
