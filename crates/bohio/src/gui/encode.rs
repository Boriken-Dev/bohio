//! Fitting a frame into a size and encoding it. Without feature `gui` no format is available and a snapshot is
//! `unsupported`.

use bohio_proto::gui::ImageFormat;

use bohio_x11::{Frame, GuiError};

/// The formats this build can encode, in the order of [`ImageFormat::ALL`].
pub(crate) fn available() -> Vec<ImageFormat> {
    if cfg!(feature = "gui") {
        ImageFormat::ALL.to_vec()
    } else {
        Vec::new()
    }
}

/// The size an image of `width` by `height` takes to fit `max_width` by `max_height`: the same proportions, never larger.
pub(crate) fn fitted(
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> (u32, u32) {
    let ratio =
        |max: Option<u32>, side: u32| max.map_or(1.0, |max| f64::from(max) / f64::from(side));
    let scale = ratio(max_width, width)
        .min(ratio(max_height, height))
        .min(1.0);
    if scale >= 1.0 {
        return (width, height);
    }
    let round = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (round(width), round(height))
}

/// `frame` scaled to `size` (a copy of it when that is its own size).
#[cfg(feature = "gui")]
pub(crate) fn resized(frame: &Frame, size: (u32, u32)) -> Result<Frame, GuiError> {
    use image::RgbImage;
    use image::imageops::{self, FilterType};

    let internal = |message| GuiError::new(bohio_proto::ErrorCode::Internal, message);
    let image = RgbImage::from_raw(frame.width(), frame.height(), frame.rgb().to_vec())
        .ok_or_else(|| internal("the picture does not match its size"))?;
    let scaled = if size == (frame.width(), frame.height()) {
        image
    } else {
        imageops::resize(&image, size.0, size.1, FilterType::Triangle)
    };
    Frame::new(size.0, size.1, scaled.into_raw())
}

#[cfg(feature = "gui")]
pub(crate) fn encode(
    frame: &Frame,
    size: (u32, u32),
    format: ImageFormat,
    quality: u8,
) -> Result<Vec<u8>, GuiError> {
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::imageops::{self, FilterType};
    use image::{ExtendedColorType, ImageEncoder, RgbImage};

    let failed = |_| {
        GuiError::new(
            bohio_proto::ErrorCode::Internal,
            "the image cannot be encoded",
        )
    };
    let image = RgbImage::from_raw(frame.width(), frame.height(), frame.rgb().to_vec())
        .ok_or_else(|| {
            GuiError::new(
                bohio_proto::ErrorCode::Internal,
                "the picture does not match its size",
            )
        })?;
    let scaled;
    let image = if size == (frame.width(), frame.height()) {
        &image
    } else {
        scaled = imageops::resize(&image, size.0, size.1, FilterType::Triangle);
        &scaled
    };
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Png => PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), size.0, size.1, ExtendedColorType::Rgb8)
            .map_err(failed)?,
        ImageFormat::Jpeg => JpegEncoder::new_with_quality(&mut bytes, quality)
            .write_image(image.as_raw(), size.0, size.1, ExtendedColorType::Rgb8)
            .map_err(failed)?,
        ImageFormat::Webp => image_webp::WebPEncoder::new(&mut bytes)
            .encode(image.as_raw(), size.0, size.1, image_webp::ColorType::Rgb8)
            .map_err(|_| {
                GuiError::new(
                    bohio_proto::ErrorCode::Internal,
                    "the image cannot be encoded",
                )
            })?,
    }
    Ok(bytes)
}

#[cfg(not(feature = "gui"))]
pub(crate) fn encode(
    _frame: &Frame,
    _size: (u32, u32),
    _format: ImageFormat,
    _quality: u8,
) -> Result<Vec<u8>, GuiError> {
    Err(GuiError::unsupported(
        "this build of the host has no image encoders",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_is_scaled_down_in_proportion_and_never_up() {
        assert_eq!(fitted(1920, 1080, Some(1280), Some(800)), (1280, 720));
        assert_eq!(fitted(1920, 1080, Some(4000), Some(4000)), (1920, 1080));
        assert_eq!(fitted(1000, 2000, Some(500), None), (500, 1000));
        assert_eq!(fitted(10, 10, Some(1), Some(1)), (1, 1));
        assert_eq!(fitted(640, 480, None, None), (640, 480));
    }
}
