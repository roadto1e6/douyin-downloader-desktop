# Third-party notices

## FFmpeg

The application embeds a compressed Windows executable at `assets/ffmpeg.exe.gz`.

- Version: `7.1-essentials_build-www.gyan.dev`.
- Original filename: `ffmpeg-win-x86_64-v7.1.exe`.
- Obtained from the binary distributed with `imageio-ffmpeg`.
- Configuration includes `--enable-gpl --enable-version3`; this FFmpeg build is under GPL version 3 or later.
- FFmpeg project and source: https://ffmpeg.org/ and https://git.ffmpeg.org/ffmpeg.git
- FFmpeg 7.1 source archive: https://ffmpeg.org/releases/ffmpeg-7.1.tar.xz
- Windows build publisher and build/source information: https://www.gyan.dev/ffmpeg/builds/
- ImageIO FFmpeg wrapper and binary provenance: https://github.com/imageio/imageio-ffmpeg
- GPL v3 license text is included in `LICENSE`.

FFmpeg is an external executable invoked for decoding and audio conversion. Python and the imageio-ffmpeg Python wrapper are not used by this Rust desktop application.

## Rust dependencies

Tauri, reqwest, tungstenite, rfd, windows-sys and other dependencies retain their original licenses. Exact versions and source package checksums are recorded in `Cargo.lock`; each crate provides its own license information in its package metadata and source distribution.
