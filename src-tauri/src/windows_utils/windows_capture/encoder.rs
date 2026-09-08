//! Image encoding for the fork's one remaining consumer: `Frame::save_as_image`, used by the
//! `--probe-capture` developer harness.
//!
//! Upstream's Media Foundation `VideoEncoder` -- `MediaStreamSource` plus `MediaTranscoder`
//! writing MP4/HEVC, about 1,300 lines -- was removed with the fork. ScreenExtend encodes with
//! NVENC, oneVPL, AMF or x264 and packetizes the result itself; it never asked Windows to mux a
//! file. See `FORK.md`.

use windows::core::HSTRING;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapEncoder, BitmapPixelFormat};
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};

#[derive(thiserror::Error, Debug)]
/// Errors that can occur when encoding raw buffers to images via [`ImageEncoder`].
pub enum ImageEncoderError {
    /// The provided source pixel format is not supported for image encoding.
    ///
    /// This occurs for formats such as [`super::settings::ColorFormat::Rgba16F`].
    #[error("This color format is not supported for saving as an image")]
    UnsupportedFormat,
    /// An I/O error occurred while writing the image to disk.
    ///
    /// Wraps [`std::io::Error`].
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    /// An integer conversion failed during buffer sizing or Windows API calls.
    ///
    /// Wraps [`std::num::TryFromIntError`].
    #[error("Integer conversion error: {0}")]
    IntConversionError(#[from] std::num::TryFromIntError),
    /// A Windows Runtime/Win32 API call failed.
    ///
    /// Wraps [`windows::core::Error`].
    #[error("Windows API error: {0}")]
    WindowsError(#[from] windows::core::Error),
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
/// Supported output image formats for [`super::encoder::ImageEncoder`].
pub enum ImageFormat {
    /// JPEG (lossy).
    Jpeg,
    /// PNG (lossless).
    Png,
    /// GIF (palette-based).
    Gif,
    /// TIFF (Tagged Image File Format).
    Tiff,
    /// BMP (Bitmap).
    Bmp,
    /// JPEG XR (HD Photo).
    JpegXr,
}

/// Pixel formats supported by the Windows API for image encoding.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum ImageEncoderPixelFormat {
    /// 16-bit floating-point RGBA format.
    Rgb16F,
    /// 8-bit unsigned integer BGRA format.
    Bgra8,
    /// 8-bit unsigned integer RGBA format.
    Rgba8,
}

/// Encodes raw image buffers into encoded bytes for common formats.
///
/// Supports saving as PNG, JPEG, GIF, TIFF, BMP, and JPEG XR when the input
/// color format is compatible.
///
/// # Example
/// ```text
/// use windows_capture::encoder::{ImageEncoder, ImageEncoderPixelFormat, ImageFormat};
///
/// let width = 320u32;
/// let height = 240u32;
/// // BGRA8 buffer (e.g., from a frame)
/// let bgra = vec![0u8; (width * height * 4) as usize];
///
/// let png_bytes = ImageEncoder::new(ImageFormat::Png, ImageEncoderPixelFormat::Bgra8)
///     .unwrap()
///     .encode(&bgra, width, height)
///     .unwrap();
///
/// std::fs::write("example.png", png_bytes).unwrap();
/// ```
pub struct ImageEncoder {
    encoder: windows::core::GUID,
    pixel_format: BitmapPixelFormat,
}

impl ImageEncoder {
    /// Constructs a new [`ImageEncoder`].
    #[inline]
    pub fn new(
        format: ImageFormat,
        pixel_format: ImageEncoderPixelFormat,
    ) -> Result<Self, ImageEncoderError> {
        let encoder = match format {
            ImageFormat::Jpeg => BitmapEncoder::JpegEncoderId()?,
            ImageFormat::Png => BitmapEncoder::PngEncoderId()?,
            ImageFormat::Gif => BitmapEncoder::GifEncoderId()?,
            ImageFormat::Tiff => BitmapEncoder::TiffEncoderId()?,
            ImageFormat::Bmp => BitmapEncoder::BmpEncoderId()?,
            ImageFormat::JpegXr => BitmapEncoder::JpegXREncoderId()?,
        };

        let pixel_format = match pixel_format {
            ImageEncoderPixelFormat::Bgra8 => BitmapPixelFormat::Bgra8,
            ImageEncoderPixelFormat::Rgba8 => BitmapPixelFormat::Rgba8,
            ImageEncoderPixelFormat::Rgb16F => BitmapPixelFormat::Rgba16,
        };

        Ok(Self {
            pixel_format,
            encoder,
        })
    }

    /// Encodes the provided pixel buffer into the configured output [`ImageFormat`].
    ///
    /// The input buffer must match the specified source [`super::settings::ColorFormat`]
    /// and dimensions. For packed 8-bit formats (e.g., [`super::settings::ColorFormat::Bgra8`]),
    /// the buffer length should be `width * height * 4`.
    ///
    /// # Errors
    ///
    /// - [`ImageEncoderError::UnsupportedFormat`] when the source format is unsupported for images
    ///   (e.g., [`super::settings::ColorFormat::Rgba16F`])
    /// - [`ImageEncoderError::WindowsError`] when Windows Imaging API calls fail
    /// - [`ImageEncoderError::IntConversionError`] on integer conversion failures
    #[inline]
    pub fn encode(
        &self,
        image_buffer: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, ImageEncoderError> {
        let stream = InMemoryRandomAccessStream::new()?;

        let encoder = BitmapEncoder::CreateAsync(self.encoder, &stream)?.join()?;

        encoder.SetPixelData(
            self.pixel_format,
            BitmapAlphaMode::Premultiplied,
            width,
            height,
            1.0,
            1.0,
            image_buffer,
        )?;
        encoder.FlushAsync()?.join()?;

        let size = stream.Size()?;
        let input = stream.GetInputStreamAt(0)?;
        let reader = DataReader::CreateDataReader(&input)?;
        reader.LoadAsync(size as u32)?.join()?;

        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes)?;

        Ok(bytes)
    }
}
