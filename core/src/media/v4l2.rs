//! v4l2loopback rawvideo output.
use crate::Result;
use anyhow::Context;
use ffmpeg_next as av;
use std::path::Path;

pub(super) struct VirtualCamera {
    output: av::format::context::Output,
    scaler: av::software::scaling::Context,
    pub(super) width: u32,
    pub(super) height: u32,
    index: i64,
}
impl VirtualCamera {
    pub(super) fn new(path: &Path, width: u32, height: u32) -> Result<Self> {
        let mut output = av::format::output_as(path, "v4l2")
            .with_context(|| format!("open virtual camera {}", path.display()))?;
        let codec =
            av::encoder::find(av::codec::Id::RAWVIDEO).context("libav rawvideo encoder missing")?;
        let mut encoder = av::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(av::format::Pixel::YUV420P);
        encoder.set_time_base((1, 30));
        let encoder = encoder.open_as(codec)?;
        {
            let mut stream = output.add_stream(codec)?;
            stream.set_parameters(&encoder);
            stream.set_time_base((1, 30));
        }
        output.write_header()?;
        let scaler = av::software::scaling::Context::get(
            av::format::Pixel::YUV420P,
            width,
            height,
            av::format::Pixel::YUV420P,
            width,
            height,
            av::software::scaling::flag::Flags::BILINEAR,
        )?;
        Ok(Self {
            output,
            scaler,
            width,
            height,
            index: 0,
        })
    }
    pub(super) fn write(&mut self, frame: &av::frame::Video) -> Result<()> {
        if self.scaler.input().format != frame.format() {
            self.scaler = av::software::scaling::Context::get(
                frame.format(),
                self.width,
                self.height,
                av::format::Pixel::YUV420P,
                self.width,
                self.height,
                av::software::scaling::flag::Flags::BILINEAR,
            )?;
        }
        let mut yuv = av::frame::Video::empty();
        self.scaler.run(frame, &mut yuv)?;
        let mut packed = Vec::with_capacity((self.width * self.height * 3 / 2) as usize);
        for plane in 0..3 {
            let (w, h) = if plane == 0 {
                (self.width, self.height)
            } else {
                (self.width.div_ceil(2), self.height.div_ceil(2))
            };
            let stride = yuv.stride(plane);
            for row in 0..h as usize {
                packed.extend_from_slice(&yuv.data(plane)[row * stride..row * stride + w as usize]);
            }
        }
        let mut packet = av::Packet::copy(&packed);
        packet.set_stream(0);
        packet.set_pts(Some(self.index));
        packet.set_dts(Some(self.index));
        self.index += 1;
        packet.write_interleaved(&mut self.output)?;
        Ok(())
    }
}
impl Drop for VirtualCamera {
    fn drop(&mut self) {
        let _ = self.output.write_trailer();
    }
}
