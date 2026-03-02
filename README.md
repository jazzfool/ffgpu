# ffgpu

a small experiment to bridge libavcodec/FFmpeg to WGPU for zero (CPU) copy GPU-accelerated video decoding.

The primary goal of this library is to bring simple and fast video playback to WGPU-based applications.

Supported platforms:

| Hardware decoder         | **Vulkan** | **DX12** | **Metal** | **OpenGL** |
|--------------------------|------------|----------|-----------|------------|
| **Windows (D3D11VA)**    | Yes        | Yes      | N/A       | CPU        |
| **MacOS (VideoToolbox)** | CPU        | N/A      | Yes       | CPU        |
| **Linux (VA-API DRM)**   | WIP        | N/A      | N/A       | CPU        |

I have no plans to support zero-copy on the OpenGL backend on any platform (but PRs for this are welcome of course).
The same goes for web and mobile platforms (Android and iOS).

This library is very incomplete and the following features are missing/WIP (roughly in order of priority):
- Playback controls (seek/pause/loop)
- Audio support (with the audio clock serving as the master clock). This includes handling A/V latency sync.
- Network streams
- Stream query and selection
- Subtitle decoding (including from a separate file)
- Software decoding fallback
- Fast thumbnailing; directly downsampled to an RGB texture atlas array from the YUV texture.
