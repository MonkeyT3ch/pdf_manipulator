//! Placement-aware image optimization for the PDF editor.

use crate::content::operators::Operator;
use crate::content::parser::parse_content_stream;
use crate::document::PdfDocument;
use crate::error::Result;
use crate::object::{Object, ObjectRef};
use std::collections::{HashMap, HashSet};
use std::io::Write;

const POINTS_PER_INCH: f64 = 72.0;
const MAX_FORM_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// JPEG chroma sampling mode.
pub enum ChromaSubsampling {
    /// Preserve full chroma resolution.
    Yuv444,
    /// Halve horizontal chroma resolution.
    Yuv422,
    /// Halve horizontal and vertical chroma resolution.
    Yuv420,
}

#[derive(Clone, Copy, Debug)]
/// Controls safe, placement-aware image replacement.
pub struct ImageOptimizationOptions {
    /// JPEG encoder quality from 1 through 100.
    pub jpeg_quality: u8,
    /// Desired effective image resolution. Zero disables resizing.
    pub target_dpi: f64,
    /// Resize only above this multiple of `target_dpi`.
    pub downsample_threshold: f64,
    /// JPEG chroma sampling mode.
    pub chroma_subsampling: ChromaSubsampling,
    /// Ignore image streams smaller than this byte count.
    pub minimum_source_bytes: usize,
    /// Required fractional byte saving before replacement.
    pub minimum_saving_ratio: f64,
    /// Preserve an existing JPEG unless it needs resizing.
    pub pass_through_jpeg: bool,
    /// Compatibility gate for the legacy quality/minSize API.
    pub legacy_min_dimension: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct Placement {
    max_width_points: f64,
    max_height_points: f64,
}

impl Placement {
    fn record(&mut self, matrix: Matrix) {
        self.max_width_points = self.max_width_points.max(matrix.x_scale());
        self.max_height_points = self.max_height_points.max(matrix.y_scale());
    }
}

#[derive(Clone, Copy, Debug)]
struct Matrix([f64; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn concat(self, left: Self) -> Self {
        let [a, b, c, d, e, f] = left.0;
        let [a0, b0, c0, d0, e0, f0] = self.0;
        Self([
            a * a0 + b * c0,
            a * b0 + b * d0,
            c * a0 + d * c0,
            c * b0 + d * d0,
            e * a0 + f * c0 + e0,
            e * b0 + f * d0 + f0,
        ])
    }

    fn x_scale(self) -> f64 {
        self.0[0].hypot(self.0[1])
    }

    fn y_scale(self) -> f64 {
        self.0[2].hypot(self.0[3])
    }
}

/// Optimizes only image objects reached from page or nested Form placements.
/// Shared image objects are resized once for their highest-resolution use.
pub fn optimize_images(
    source: &PdfDocument,
    modified_objects: &mut HashMap<u32, Object>,
    options: &ImageOptimizationOptions,
) -> Result<usize> {
    let placements = collect_placements(source)?;
    let mut count = 0;

    for (object_id, placement) in placements {
        let object_ref = ObjectRef::new(object_id, 0);
        let object = match source.load_object(object_ref) {
            Ok(object) => object,
            Err(_) => continue,
        };
        let (dict, raw_data) = match object {
            Object::Stream { dict, data } => (dict, data.to_vec()),
            _ => continue,
        };
        if raw_data.len() < options.minimum_source_bytes {
            continue;
        }

        let width = match positive_u32(dict.get("Width")) {
            Some(value) => value,
            None => continue,
        };
        let height = match positive_u32(dict.get("Height")) {
            Some(value) => value,
            None => continue,
        };
        if options.legacy_min_dimension > 0
            && (width < options.legacy_min_dimension || height < options.legacy_min_dimension)
        {
            continue;
        }
        if dict
            .get("BitsPerComponent")
            .and_then(Object::as_integer)
            .unwrap_or(8)
            != 8
        {
            continue;
        }
        // Decode arrays can invert channels or apply non-default ranges.
        if dict.contains_key("Decode") || dict.contains_key("Mask") {
            continue;
        }

        let components = match resolved_color_space_name(source, &dict).as_deref() {
            Some("DeviceGray") => 1,
            Some("DeviceRGB") => 3,
            _ => continue,
        };
        let filter = match single_filter(&dict) {
            Some(filter) => filter,
            None => continue,
        };
        if !matches!(filter, "" | "FlateDecode" | "DCTDecode") {
            continue;
        }

        let (new_width, new_height) = target_dimensions(width, height, placement, options);
        let resized = new_width < width || new_height < height;
        if filter == "DCTDecode" && options.pass_through_jpeg && !resized {
            continue;
        }

        let decoded = match decode_image_pixels(&dict, &raw_data, filter, width, height, components)
        {
            Some(decoded) => decoded,
            None => continue,
        };
        let expected = width as usize * height as usize * components;
        if decoded.len() < expected {
            continue;
        }
        let pixels = if resized {
            match resize_pixels(
                &decoded[..expected],
                width,
                height,
                new_width,
                new_height,
                components,
            ) {
                Some(pixels) => pixels,
                None => continue,
            }
        } else {
            decoded[..expected].to_vec()
        };

        let lossless = filter != "DCTDecode" && looks_flat(&pixels, components);
        let (candidate, candidate_filter) = if lossless {
            match deflate(&pixels) {
                Some(candidate) => (candidate, "FlateDecode"),
                None => continue,
            }
        } else {
            match encode_jpeg(
                &pixels,
                new_width,
                new_height,
                components,
                options.jpeg_quality,
                options.chroma_subsampling,
            ) {
                Some(candidate) => (candidate, "DCTDecode"),
                None => continue,
            }
        };

        let mut mask_update = None;
        let mut original_total = raw_data.len();
        let mut candidate_total = candidate.len();
        if resized {
            if let Some(mask_ref) = dict.get("SMask").and_then(Object::as_reference) {
                match resized_soft_mask(source, mask_ref, width, height, new_width, new_height) {
                    Some((mask, old_size, new_size)) => {
                        original_total += old_size;
                        candidate_total += new_size;
                        mask_update = Some((mask_ref.id, mask));
                    },
                    None => continue,
                }
            }
        }
        if !beats_minimum_saving(candidate_total, original_total, options.minimum_saving_ratio) {
            continue;
        }

        let mut new_dict = dict;
        new_dict.insert("Width".into(), Object::Integer(new_width as i64));
        new_dict.insert("Height".into(), Object::Integer(new_height as i64));
        new_dict.insert("Length".into(), Object::Integer(candidate.len() as i64));
        new_dict.insert("Filter".into(), Object::Name(candidate_filter.into()));
        new_dict.remove("DecodeParms");
        modified_objects.insert(
            object_id,
            Object::Stream {
                dict: new_dict,
                data: bytes::Bytes::from(candidate),
            },
        );
        if let Some((mask_id, mask)) = mask_update {
            modified_objects.insert(mask_id, mask);
        }
        count += 1;
    }

    Ok(count)
}

fn collect_placements(source: &PdfDocument) -> Result<HashMap<u32, Placement>> {
    let mut placements = HashMap::new();
    for page_index in 0..source.page_count()? {
        let content = source.get_page_content_data(page_index)?;
        let resources = source.get_page_resources(page_index)?;
        walk_content(
            source,
            &content,
            &resources,
            Matrix::IDENTITY,
            0,
            &mut Vec::new(),
            &mut placements,
        );
    }
    Ok(placements)
}

fn walk_content(
    source: &PdfDocument,
    content: &[u8],
    resources: &Object,
    initial_ctm: Matrix,
    depth: usize,
    form_stack: &mut Vec<u32>,
    placements: &mut HashMap<u32, Placement>,
) {
    if depth > MAX_FORM_DEPTH {
        return;
    }
    let operators = match parse_content_stream(content) {
        Ok(operators) => operators,
        Err(_) => return,
    };
    let resources = match source.resolve_object(resources) {
        Ok(resources) => resources,
        Err(_) => return,
    };
    let xobjects = match resources
        .as_dict()
        .and_then(|dict| dict.get("XObject"))
        .and_then(|object| source.resolve_object(object).ok())
    {
        Some(object) => object,
        None => return,
    };
    let xobjects = match xobjects.as_dict() {
        Some(dict) => dict,
        None => return,
    };

    let mut ctm = initial_ctm;
    let mut stack = Vec::new();
    for operator in operators {
        match operator {
            Operator::SaveState => stack.push(ctm),
            Operator::RestoreState => {
                if let Some(saved) = stack.pop() {
                    ctm = saved;
                }
            },
            Operator::Cm { a, b, c, d, e, f } => {
                ctm = ctm
                    .concat(Matrix([a as f64, b as f64, c as f64, d as f64, e as f64, f as f64]));
            },
            Operator::Do { name } => {
                let resource = match xobjects.get(&name) {
                    Some(resource) => resource,
                    None => continue,
                };
                let object_ref = resource.as_reference();
                let object = match source.resolve_object(resource) {
                    Ok(object) => object,
                    Err(_) => continue,
                };
                let (dict, data) = match object {
                    Object::Stream { dict, data } => (dict, data),
                    _ => continue,
                };
                match dict.get("Subtype").and_then(Object::as_name) {
                    Some("Image") => {
                        if let Some(object_ref) = object_ref {
                            placements.entry(object_ref.id).or_default().record(ctm);
                        }
                    },
                    Some("Form") => {
                        let form_id = object_ref.map(|value| value.id);
                        if form_id.is_some_and(|id| form_stack.contains(&id)) {
                            continue;
                        }
                        let form_matrix = dict
                            .get("Matrix")
                            .and_then(matrix_from_object)
                            .unwrap_or(Matrix::IDENTITY);
                        let form_resources = dict.get("Resources").unwrap_or(&resources);
                        if let Some(id) = form_id {
                            form_stack.push(id);
                        }
                        let decoded = Object::Stream {
                            dict: dict.clone(),
                            data,
                        }
                        .decode_stream_data();
                        if let Ok(decoded) = decoded {
                            walk_content(
                                source,
                                &decoded,
                                form_resources,
                                ctm.concat(form_matrix),
                                depth + 1,
                                form_stack,
                                placements,
                            );
                        }
                        if form_id.is_some() {
                            form_stack.pop();
                        }
                    },
                    _ => {},
                }
            },
            _ => {},
        }
    }
}

fn matrix_from_object(object: &Object) -> Option<Matrix> {
    let values = object.as_array()?;
    if values.len() != 6 {
        return None;
    }
    let mut matrix = [0.0; 6];
    for (index, value) in values.iter().enumerate() {
        matrix[index] = value
            .as_real()
            .or_else(|| value.as_integer().map(|number| number as f64))?;
    }
    Some(Matrix(matrix))
}

fn target_dimensions(
    width: u32,
    height: u32,
    placement: Placement,
    options: &ImageOptimizationOptions,
) -> (u32, u32) {
    if options.target_dpi <= 0.0
        || placement.max_width_points <= 0.0
        || placement.max_height_points <= 0.0
    {
        return (width, height);
    }
    let horizontal_dpi = width as f64 * POINTS_PER_INCH / placement.max_width_points;
    let vertical_dpi = height as f64 * POINTS_PER_INCH / placement.max_height_points;
    if horizontal_dpi <= options.target_dpi * options.downsample_threshold
        && vertical_dpi <= options.target_dpi * options.downsample_threshold
    {
        return (width, height);
    }
    let target_width = (placement.max_width_points * options.target_dpi / POINTS_PER_INCH).ceil();
    let target_height = (placement.max_height_points * options.target_dpi / POINTS_PER_INCH).ceil();
    let scale = (target_width / width as f64)
        .max(target_height / height as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

fn resize_pixels(
    pixels: &[u8],
    width: u32,
    height: u32,
    new_width: u32,
    new_height: u32,
    components: usize,
) -> Option<Vec<u8>> {
    use image::imageops::FilterType;
    match components {
        1 => {
            let image = image::GrayImage::from_raw(width, height, pixels.to_vec())?;
            Some(
                image::imageops::resize(&image, new_width, new_height, FilterType::Lanczos3)
                    .into_raw(),
            )
        },
        3 => {
            let image = image::RgbImage::from_raw(width, height, pixels.to_vec())?;
            Some(
                image::imageops::resize(&image, new_width, new_height, FilterType::Lanczos3)
                    .into_raw(),
            )
        },
        _ => None,
    }
}

fn decode_image_pixels(
    dict: &HashMap<String, Object>,
    raw_data: &[u8],
    filter: &str,
    width: u32,
    height: u32,
    components: usize,
) -> Option<Vec<u8>> {
    if filter == "DCTDecode" {
        let decoded =
            image::load_from_memory_with_format(raw_data, image::ImageFormat::Jpeg).ok()?;
        return match components {
            1 => {
                let pixels = decoded.to_luma8();
                (pixels.dimensions() == (width, height)).then(|| pixels.into_raw())
            },
            3 => {
                let pixels = decoded.to_rgb8();
                (pixels.dimensions() == (width, height)).then(|| pixels.into_raw())
            },
            _ => None,
        };
    }

    (Object::Stream {
        dict: dict.clone(),
        data: bytes::Bytes::copy_from_slice(raw_data),
    })
    .decode_stream_data()
    .ok()
}

fn encode_jpeg(
    pixels: &[u8],
    width: u32,
    height: u32,
    components: usize,
    quality: u8,
    subsampling: ChromaSubsampling,
) -> Option<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let width = u16::try_from(width).ok()?;
    let height = u16::try_from(height).ok()?;
    let mut output = Vec::new();
    let mut encoder = Encoder::new(&mut output, quality);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_sampling_factor(match subsampling {
        ChromaSubsampling::Yuv444 => SamplingFactor::R_4_4_4,
        ChromaSubsampling::Yuv422 => SamplingFactor::R_4_2_2,
        ChromaSubsampling::Yuv420 => SamplingFactor::R_4_2_0,
    });
    encoder
        .encode(
            pixels,
            width,
            height,
            if components == 1 {
                ColorType::Luma
            } else {
                ColorType::Rgb
            },
        )
        .ok()?;
    Some(output)
}

fn resized_soft_mask(
    source: &PdfDocument,
    mask_ref: ObjectRef,
    parent_width: u32,
    parent_height: u32,
    new_width: u32,
    new_height: u32,
) -> Option<(Object, usize, usize)> {
    let mask = source.load_object(mask_ref).ok()?;
    let (mut dict, data) = match mask {
        Object::Stream { dict, data } => (dict, data.to_vec()),
        _ => return None,
    };
    if positive_u32(dict.get("Width"))? != parent_width
        || positive_u32(dict.get("Height"))? != parent_height
        || dict
            .get("BitsPerComponent")
            .and_then(Object::as_integer)
            .unwrap_or(8)
            != 8
        || resolved_color_space_name(source, &dict).as_deref() != Some("DeviceGray")
        || dict.contains_key("Decode")
    {
        return None;
    }
    let decoded = Object::Stream {
        dict: dict.clone(),
        data: bytes::Bytes::from(data.clone()),
    }
    .decode_stream_data()
    .ok()?;
    let resized = resize_pixels(&decoded, parent_width, parent_height, new_width, new_height, 1)?;
    let encoded = deflate(&resized)?;
    let encoded_len = encoded.len();
    dict.insert("Width".into(), Object::Integer(new_width as i64));
    dict.insert("Height".into(), Object::Integer(new_height as i64));
    dict.insert("Length".into(), Object::Integer(encoded_len as i64));
    dict.insert("Filter".into(), Object::Name("FlateDecode".into()));
    dict.remove("DecodeParms");
    Some((
        Object::Stream {
            dict,
            data: bytes::Bytes::from(encoded),
        },
        data.len(),
        encoded_len,
    ))
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

fn looks_flat(pixels: &[u8], components: usize) -> bool {
    let pixel_count = pixels.len() / components;
    if pixel_count == 0 {
        return true;
    }
    let stride = (pixel_count / 4096).max(1);
    let limit = if components == 1 { 64 } else { 256 };
    let mut colors = HashSet::new();
    for pixel_index in (0..pixel_count).step_by(stride) {
        let offset = pixel_index * components;
        let color = if components == 1 {
            pixels[offset] as u32
        } else {
            ((pixels[offset] as u32) << 16)
                | ((pixels[offset + 1] as u32) << 8)
                | pixels[offset + 2] as u32
        };
        colors.insert(color);
        if colors.len() > limit {
            return false;
        }
    }
    true
}

fn single_filter(dict: &HashMap<String, Object>) -> Option<&str> {
    match dict.get("Filter") {
        None => Some(""),
        Some(Object::Name(name)) => Some(name),
        Some(Object::Array(filters)) if filters.len() == 1 => filters[0].as_name(),
        _ => None,
    }
}

fn resolved_color_space_name(
    source: &PdfDocument,
    dict: &HashMap<String, Object>,
) -> Option<String> {
    let color_space = source.resolve_object(dict.get("ColorSpace")?).ok()?;
    if let Some(name) = color_space.as_name() {
        return Some(name.to_owned());
    }

    let values = color_space.as_array()?;
    if values.first().and_then(Object::as_name) != Some("ICCBased") {
        return None;
    }
    let profile = source.resolve_object(values.get(1)?).ok()?;
    match profile.as_dict()?.get("N").and_then(Object::as_integer) {
        Some(1) => Some("DeviceGray".to_owned()),
        Some(3) => Some("DeviceRGB".to_owned()),
        _ => None,
    }
}

fn positive_u32(object: Option<&Object>) -> Option<u32> {
    u32::try_from(object?.as_integer()?)
        .ok()
        .filter(|value| *value > 0)
}

fn beats_minimum_saving(candidate: usize, original: usize, minimum_ratio: f64) -> bool {
    candidate < original && candidate as f64 <= original as f64 * (1.0 - minimum_ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_pixels(width: u32, height: u32, components: usize) -> (Vec<u8>, Vec<u8>) {
        let source = (0..width * height * components as u32)
            .map(|index| (index.wrapping_mul(37) & 0xff) as u8)
            .collect::<Vec<_>>();
        let jpeg = encode_jpeg(&source, width, height, components, 90, ChromaSubsampling::Yuv444)
            .expect("encode JPEG fixture");
        (source, jpeg)
    }

    fn options(target_dpi: f64) -> ImageOptimizationOptions {
        ImageOptimizationOptions {
            jpeg_quality: 75,
            target_dpi,
            downsample_threshold: 1.5,
            chroma_subsampling: ChromaSubsampling::Yuv422,
            minimum_source_bytes: 0,
            minimum_saving_ratio: 0.01,
            pass_through_jpeg: true,
            legacy_min_dimension: 0,
        }
    }

    #[test]
    fn threshold_boundary_does_not_resize() {
        let placement = Placement {
            max_width_points: 480.0,
            max_height_points: 240.0,
        };
        assert_eq!(target_dimensions(1500, 750, placement, &options(150.0)), (1500, 750));
    }

    #[test]
    fn above_threshold_resizes_without_upscaling() {
        let placement = Placement {
            max_width_points: 360.0,
            max_height_points: 180.0,
        };
        assert_eq!(target_dimensions(1501, 751, placement, &options(150.0)), (750, 375));
    }

    #[test]
    fn largest_shared_placement_controls_dimensions() {
        let placement = Placement {
            max_width_points: 720.0,
            max_height_points: 360.0,
        };
        assert_eq!(target_dimensions(3000, 1500, placement, &options(150.0)), (1500, 750));
    }

    #[test]
    fn one_percent_guard_is_inclusive() {
        assert!(beats_minimum_saving(990, 1000, 0.01));
        assert!(!beats_minimum_saving(991, 1000, 0.01));
    }

    #[test]
    fn dct_image_streams_are_decoded_to_pixels_before_optimization() {
        let (source, jpeg) = jpeg_pixels(16, 8, 3);
        let decoded = decode_image_pixels(&HashMap::new(), &jpeg, "DCTDecode", 16, 8, 3)
            .expect("decode JPEG image stream");

        assert_eq!(decoded.len(), source.len());
        assert_ne!(decoded, jpeg);
    }
}
