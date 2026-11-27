//! Clean-room Prusa Binary G-code (`.bgcode` v1) encoder and decoder.
//!
//! Format layout (little-endian throughout):
//! - 10-byte file header: magic `b"GCDE"`, `version: u32 = 1`, `checksum_type: u16` (`0` None, `1` CRC32).
//! - Sequence of blocks: `[header][parameters][data][optional crc32]`
//!   - Header: `type: u16`, `compression: u16`, `uncompressed_size: u32`, optional `compressed_size: u32`
//!     (the 4-byte `compressed_size` field is present only when `compression != 0`).
//!   - Block parameters (uncompressed):
//!     - Metadata blocks (`0, 2, 3, 4`): `encoding: u16 = 0` (INI).
//!     - GCode blocks (`1`): `encoding: u16` (`0 = None`).
//!     - Thumbnail blocks (`5`): `format: u16` (`0=PNG`, `1=JPG`, `2=QOI`), `width: u16`, `height: u16`.
//!   - Checksum (when `checksum_type == 1`): IEEE CRC32 over `[header || parameters || stored_data]`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};

use base64::Engine as _;
use flate2::Compression as FlateLevel;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use layercraft_config::{GcodeFlavor, PrintConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::GcodeOutput;

const BGCODE_MAGIC: &[u8; 4] = b"GCDE";
const BGCODE_VERSION: u32 = 1;
const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_GCODE_CHUNK_BYTES: usize = 65_536;
const MIN_GCODE_CHUNK_BYTES: usize = 4_096;

/// Supported `.bgcode` block payload compression algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BgcodeCompression {
    None,
    Deflate,
    #[default]
    #[serde(alias = "heatshrink_11_4")]
    Heatshrink11_4,
    #[serde(alias = "heatshrink_12_4")]
    Heatshrink12_4,
}

impl BgcodeCompression {
    fn to_u16(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Deflate => 1,
            Self::Heatshrink11_4 => 2,
            Self::Heatshrink12_4 => 3,
        }
    }

    fn from_u16(raw: u16) -> Result<Self, BgcodeError> {
        match raw {
            0 => Ok(Self::None),
            1 => Ok(Self::Deflate),
            2 => Ok(Self::Heatshrink11_4),
            3 => Ok(Self::Heatshrink12_4),
            other => Err(BgcodeError::UnsupportedCompression(other)),
        }
    }
}

/// Block integrity checksum mode stored in the `.bgcode` file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BgcodeChecksum {
    None,
    #[default]
    Crc32,
}

impl BgcodeChecksum {
    fn to_u16(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Crc32 => 1,
        }
    }

    fn from_u16(raw: u16) -> Result<Self, BgcodeError> {
        match raw {
            0 => Ok(Self::None),
            1 => Ok(Self::Crc32),
            other => Err(BgcodeError::UnsupportedChecksum(other)),
        }
    }
}

/// Thumbnail image format stored inside a `.bgcode` thumbnail block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BgcodeThumbnailFormat {
    Png,
    Jpg,
    Qoi,
}

impl BgcodeThumbnailFormat {
    fn to_u16(self) -> u16 {
        match self {
            Self::Png => 0,
            Self::Jpg => 1,
            Self::Qoi => 2,
        }
    }

    fn from_u16(raw: u16) -> Result<Self, BgcodeError> {
        match raw {
            0 => Ok(Self::Png),
            1 => Ok(Self::Jpg),
            2 => Ok(Self::Qoi),
            other => Err(BgcodeError::UnsupportedThumbnailFormat(other)),
        }
    }
}

/// Encoding options controlling `.bgcode` compression, checksums, and G-code chunking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct BgcodeOptions {
    pub compression: BgcodeCompression,
    pub metadata_compression: BgcodeCompression,
    pub checksum: BgcodeChecksum,
    pub gcode_chunk_bytes: usize,
}

impl Default for BgcodeOptions {
    fn default() -> Self {
        Self {
            compression: BgcodeCompression::Heatshrink11_4,
            metadata_compression: BgcodeCompression::None,
            checksum: BgcodeChecksum::Crc32,
            gcode_chunk_bytes: DEFAULT_GCODE_CHUNK_BYTES,
        }
    }
}

/// Decoded embedded thumbnail block from a `.bgcode` container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgcodeThumbnail {
    pub format: BgcodeThumbnailFormat,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Decoded contents of a `.bgcode` container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgcodeDocument {
    pub version: u32,
    pub checksum: BgcodeChecksum,
    pub file_metadata: BTreeMap<String, String>,
    pub printer_metadata: BTreeMap<String, String>,
    pub print_metadata: BTreeMap<String, String>,
    pub slicer_metadata: BTreeMap<String, String>,
    pub thumbnails: Vec<BgcodeThumbnail>,
    pub gcode: String,
    pub gcode_block_count: usize,
}

/// Errors returned while encoding or decoding `.bgcode` files.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BgcodeError {
    #[error("input is not a .bgcode file (missing GCDE magic header)")]
    InvalidMagic,
    #[error("unsupported .bgcode format version: {0}")]
    UnsupportedVersion(u32),
    #[error("unsupported .bgcode checksum type: {0}")]
    UnsupportedChecksum(u16),
    #[error("unsupported .bgcode block compression id: {0}")]
    UnsupportedCompression(u16),
    #[error("unsupported .bgcode block encoding id: {0}")]
    UnsupportedEncoding(u16),
    #[error("unsupported .bgcode thumbnail format id: {0}")]
    UnsupportedThumbnailFormat(u16),
    #[error("unknown .bgcode block type id: {0}")]
    UnknownBlockType(u16),
    #[error("truncated .bgcode file or block")]
    Truncated,
    #[error(
        ".bgcode CRC32 mismatch on block {block_index}: expected {expected:#010x}, got {actual:#010x}"
    )]
    ChecksumMismatch {
        block_index: usize,
        expected: u32,
        actual: u32,
    },
    #[error(".bgcode block {block_index} exceeds maximum size limit ({size} bytes)")]
    BlockTooLarge { block_index: usize, size: usize },
    #[error(".bgcode compression failed: {0}")]
    CompressionFailed(&'static str),
    #[error(".bgcode decompression failed: {0}")]
    DecompressionFailed(&'static str),
    #[error(".bgcode payload contains invalid UTF-8")]
    InvalidUtf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockType {
    FileMetadata = 0,
    GCode = 1,
    SlicerMetadata = 2,
    PrinterMetadata = 3,
    PrintMetadata = 4,
    Thumbnail = 5,
}

impl BlockType {
    fn from_u16(raw: u16) -> Result<Self, BgcodeError> {
        match raw {
            0 => Ok(Self::FileMetadata),
            1 => Ok(Self::GCode),
            2 => Ok(Self::SlicerMetadata),
            3 => Ok(Self::PrinterMetadata),
            4 => Ok(Self::PrintMetadata),
            5 => Ok(Self::Thumbnail),
            other => Err(BgcodeError::UnknownBlockType(other)),
        }
    }
}

/// Return `true` when `bytes` begins with the `.bgcode` `GCDE` magic header.
pub fn is_bgcode(bytes: &[u8]) -> bool {
    bytes.starts_with(BGCODE_MAGIC)
}

/// Encode a [`GcodeOutput`] and [`PrintConfig`] into a Binary G-code (`.bgcode`) byte stream.
pub fn encode_bgcode(
    output: &GcodeOutput,
    config: &PrintConfig,
    options: &BgcodeOptions,
) -> Result<Vec<u8>, BgcodeError> {
    let (cleaned_gcode, extracted_thumbs) = extract_gcode_thumbnails(&output.gcode);

    let mut out = Vec::with_capacity(cleaned_gcode.len() / 2 + 4096);
    out.extend_from_slice(BGCODE_MAGIC);
    out.extend_from_slice(&BGCODE_VERSION.to_le_bytes());
    out.extend_from_slice(&options.checksum.to_u16().to_le_bytes());

    let ini_params = 0u16.to_le_bytes();

    // 1. File Metadata (Type 0)
    let file_ini = "Producer=LayerCraft 0.1.0\n";
    write_block(
        &mut out,
        BlockType::FileMetadata,
        options.metadata_compression,
        options.checksum,
        &ini_params,
        file_ini.as_bytes(),
    )?;

    // 2. Printer Metadata (Type 3)
    let printer_ini = build_printer_ini(output, config);
    if !printer_ini.is_empty() {
        write_block(
            &mut out,
            BlockType::PrinterMetadata,
            options.metadata_compression,
            options.checksum,
            &ini_params,
            printer_ini.as_bytes(),
        )?;
    }

    // 3. Thumbnail Blocks (Type 5, stored uncompressed since PNG/JPG/QOI are already compressed)
    for thumb in &extracted_thumbs {
        let mut params = [0u8; 6];
        let fmt_le = thumb.format.to_u16().to_le_bytes();
        let w_le = thumb.width.to_le_bytes();
        let h_le = thumb.height.to_le_bytes();
        if let Some(dst) = params.get_mut(0..2) {
            dst.copy_from_slice(&fmt_le);
        }
        if let Some(dst) = params.get_mut(2..4) {
            dst.copy_from_slice(&w_le);
        }
        if let Some(dst) = params.get_mut(4..6) {
            dst.copy_from_slice(&h_le);
        }
        write_block(
            &mut out,
            BlockType::Thumbnail,
            BgcodeCompression::None,
            options.checksum,
            &params,
            &thumb.data,
        )?;
    }

    // 4. Print Metadata (Type 4)
    let print_ini = build_print_ini(output, config);
    if !print_ini.is_empty() {
        write_block(
            &mut out,
            BlockType::PrintMetadata,
            options.metadata_compression,
            options.checksum,
            &ini_params,
            print_ini.as_bytes(),
        )?;
    }

    // 5. Slicer Metadata (Type 2)
    let slicer_ini = build_slicer_ini(config);
    write_block(
        &mut out,
        BlockType::SlicerMetadata,
        options.metadata_compression,
        options.checksum,
        &ini_params,
        slicer_ini.as_bytes(),
    )?;

    // 6. GCode Blocks (Type 1) chunked along newline boundaries
    let gcode_params = 0u16.to_le_bytes();
    let max_chunk = options.gcode_chunk_bytes.max(MIN_GCODE_CHUNK_BYTES);
    let chunks = split_gcode_chunks(&cleaned_gcode, max_chunk);
    if chunks.is_empty() {
        write_block(
            &mut out,
            BlockType::GCode,
            BgcodeCompression::None,
            options.checksum,
            &gcode_params,
            b"",
        )?;
    } else {
        for chunk in chunks {
            write_block(
                &mut out,
                BlockType::GCode,
                options.compression,
                options.checksum,
                &gcode_params,
                chunk.as_bytes(),
            )?;
        }
    }

    Ok(out)
}

/// Decode and strictly verify a `.bgcode` container back into metadata, thumbnails, and G-code.
pub fn decode_bgcode(bytes: &[u8]) -> Result<BgcodeDocument, BgcodeError> {
    if !is_bgcode(bytes) {
        return Err(BgcodeError::InvalidMagic);
    }
    if bytes.len() < 10 {
        return Err(BgcodeError::Truncated);
    }

    let version = read_u32_le(bytes, 4)?;
    if version != BGCODE_VERSION {
        return Err(BgcodeError::UnsupportedVersion(version));
    }
    let checksum = BgcodeChecksum::from_u16(read_u16_le(bytes, 8)?)?;

    let mut cursor = 10usize;
    let mut block_index = 0usize;
    let mut file_metadata = BTreeMap::new();
    let mut printer_metadata = BTreeMap::new();
    let mut print_metadata = BTreeMap::new();
    let mut slicer_metadata = BTreeMap::new();
    let mut thumbnails = Vec::new();
    let mut gcode = String::new();
    let mut gcode_block_count = 0usize;

    while cursor < bytes.len() {
        let block_start = cursor;
        if cursor + 8 > bytes.len() {
            return Err(BgcodeError::Truncated);
        }
        let block_type = BlockType::from_u16(read_u16_le(bytes, cursor)?)?;
        let compression = BgcodeCompression::from_u16(read_u16_le(bytes, cursor + 2)?)?;
        let uncompressed_size = read_u32_le(bytes, cursor + 4)? as usize;
        if uncompressed_size > MAX_BLOCK_BYTES {
            return Err(BgcodeError::BlockTooLarge {
                block_index,
                size: uncompressed_size,
            });
        }
        cursor += 8;

        let stored_size = if compression == BgcodeCompression::None {
            uncompressed_size
        } else {
            if cursor + 4 > bytes.len() {
                return Err(BgcodeError::Truncated);
            }
            let c_size = read_u32_le(bytes, cursor)? as usize;
            if c_size > MAX_BLOCK_BYTES {
                return Err(BgcodeError::BlockTooLarge {
                    block_index,
                    size: c_size,
                });
            }
            cursor += 4;
            c_size
        };

        // Block parameters (uncompressed)
        let (param_len, thumb_params, gcode_enc) = match block_type {
            BlockType::FileMetadata
            | BlockType::PrinterMetadata
            | BlockType::PrintMetadata
            | BlockType::SlicerMetadata => {
                if cursor + 2 > bytes.len() {
                    return Err(BgcodeError::Truncated);
                }
                let enc = read_u16_le(bytes, cursor)?;
                if enc != 0 {
                    return Err(BgcodeError::UnsupportedEncoding(enc));
                }
                (2usize, None, 0u16)
            }
            BlockType::GCode => {
                if cursor + 2 > bytes.len() {
                    return Err(BgcodeError::Truncated);
                }
                let enc = read_u16_le(bytes, cursor)?;
                (2usize, None, enc)
            }
            BlockType::Thumbnail => {
                if cursor + 6 > bytes.len() {
                    return Err(BgcodeError::Truncated);
                }
                let fmt = BgcodeThumbnailFormat::from_u16(read_u16_le(bytes, cursor)?)?;
                let w = read_u16_le(bytes, cursor + 2)?;
                let h = read_u16_le(bytes, cursor + 4)?;
                (6usize, Some((fmt, w, h)), 0u16)
            }
        };
        cursor += param_len;

        let data_end = cursor
            .checked_add(stored_size)
            .ok_or(BgcodeError::Truncated)?;
        let stored_data = bytes.get(cursor..data_end).ok_or(BgcodeError::Truncated)?;
        cursor = data_end;

        if checksum == BgcodeChecksum::Crc32 {
            let expected = read_u32_le(bytes, cursor)?;
            let covered = bytes
                .get(block_start..data_end)
                .ok_or(BgcodeError::Truncated)?;
            let actual = crc32fast::hash(covered);
            if actual != expected {
                return Err(BgcodeError::ChecksumMismatch {
                    block_index,
                    expected,
                    actual,
                });
            }
            cursor += 4;
        }

        let payload = decompress_payload(compression, stored_data, uncompressed_size)?;

        match block_type {
            BlockType::FileMetadata => {
                let text = std::str::from_utf8(&payload).map_err(|_| BgcodeError::InvalidUtf8)?;
                parse_ini_into(text, &mut file_metadata);
            }
            BlockType::PrinterMetadata => {
                let text = std::str::from_utf8(&payload).map_err(|_| BgcodeError::InvalidUtf8)?;
                parse_ini_into(text, &mut printer_metadata);
            }
            BlockType::PrintMetadata => {
                let text = std::str::from_utf8(&payload).map_err(|_| BgcodeError::InvalidUtf8)?;
                parse_ini_into(text, &mut print_metadata);
            }
            BlockType::SlicerMetadata => {
                let text = std::str::from_utf8(&payload).map_err(|_| BgcodeError::InvalidUtf8)?;
                parse_ini_into(text, &mut slicer_metadata);
            }
            BlockType::Thumbnail => {
                if let Some((format, width, height)) = thumb_params {
                    thumbnails.push(BgcodeThumbnail {
                        format,
                        width,
                        height,
                        data: payload,
                    });
                }
            }
            BlockType::GCode => {
                if gcode_enc != 0 {
                    return Err(BgcodeError::UnsupportedEncoding(gcode_enc));
                }
                let text = std::str::from_utf8(&payload).map_err(|_| BgcodeError::InvalidUtf8)?;
                gcode.push_str(text);
                gcode_block_count += 1;
            }
        }

        block_index += 1;
    }

    Ok(BgcodeDocument {
        version,
        checksum,
        file_metadata,
        printer_metadata,
        print_metadata,
        slicer_metadata,
        thumbnails,
        gcode,
        gcode_block_count,
    })
}

fn write_block(
    out: &mut Vec<u8>,
    block_type: BlockType,
    requested_compression: BgcodeCompression,
    checksum: BgcodeChecksum,
    params: &[u8],
    raw_data: &[u8],
) -> Result<(), BgcodeError> {
    let uncompressed_u32 =
        u32::try_from(raw_data.len()).map_err(|_| BgcodeError::BlockTooLarge {
            block_index: 0,
            size: raw_data.len(),
        })?;

    let (actual_compression, stored_cow): (BgcodeCompression, std::borrow::Cow<'_, [u8]>) =
        if raw_data.is_empty() || requested_compression == BgcodeCompression::None {
            (
                BgcodeCompression::None,
                std::borrow::Cow::Borrowed(raw_data),
            )
        } else {
            let compressed = compress_payload(requested_compression, raw_data)?;
            if compressed.len() < raw_data.len() {
                (requested_compression, std::borrow::Cow::Owned(compressed))
            } else {
                (
                    BgcodeCompression::None,
                    std::borrow::Cow::Borrowed(raw_data),
                )
            }
        };

    let block_start = out.len();
    out.extend_from_slice(&(block_type as u16).to_le_bytes());
    out.extend_from_slice(&actual_compression.to_u16().to_le_bytes());
    out.extend_from_slice(&uncompressed_u32.to_le_bytes());
    if actual_compression != BgcodeCompression::None {
        let comp_u32 = u32::try_from(stored_cow.len()).map_err(|_| BgcodeError::BlockTooLarge {
            block_index: 0,
            size: stored_cow.len(),
        })?;
        out.extend_from_slice(&comp_u32.to_le_bytes());
    }
    out.extend_from_slice(params);
    out.extend_from_slice(&stored_cow);

    if checksum == BgcodeChecksum::Crc32 {
        let covered = out
            .get(block_start..)
            .ok_or(BgcodeError::CompressionFailed("crc slice"))?;
        let crc = crc32fast::hash(covered);
        out.extend_from_slice(&crc.to_le_bytes());
    }

    Ok(())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, BgcodeError> {
    let end = offset.checked_add(2).ok_or(BgcodeError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(BgcodeError::Truncated)?;
    let arr: [u8; 2] = slice.try_into().map_err(|_| BgcodeError::Truncated)?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, BgcodeError> {
    let end = offset.checked_add(4).ok_or(BgcodeError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(BgcodeError::Truncated)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| BgcodeError::Truncated)?;
    Ok(u32::from_le_bytes(arr))
}

fn build_printer_ini(output: &GcodeOutput, config: &PrintConfig) -> String {
    let mut s = String::new();
    if let Some(pm) = config
        .machine
        .printer_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let _ = writeln!(s, "printer_model={pm}");
    }
    if let Some(ft) = config
        .filament
        .filament_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let _ = writeln!(s, "filament_type={ft}");
    }
    let _ = writeln!(s, "filament_abrasive=0");
    let _ = writeln!(
        s,
        "nozzle_diameter={}",
        crate::format_compact_f64(config.machine.nozzle_diameter_mm)
    );
    let _ = writeln!(
        s,
        "nozzle_high_flow={}",
        u8::from(config.machine.nozzle_high_flow)
    );
    let _ = writeln!(s, "bed_temperature={}", config.filament.bed_temp_c);
    let _ = writeln!(
        s,
        "brim_width={}",
        crate::format_compact_f64(config.process.brim_width_mm)
    );
    let _ = writeln!(
        s,
        "fill_density={:.0}%",
        config.process.sparse_infill_density * 100.0
    );
    let _ = writeln!(
        s,
        "layer_height={}",
        crate::format_compact_f64(config.process.layer_height_mm)
    );
    let _ = writeln!(s, "temperature={}", config.filament.nozzle_temp_c);
    let _ = writeln!(
        s,
        "ironing={}",
        u8::from(config.process.ironing.mode != layercraft_config::IroningType::Off)
    );
    let _ = writeln!(
        s,
        "support_material={}",
        u8::from(config.process.support.enabled)
    );
    let _ = writeln!(s, "max_layer_z={:.2}", output.stats.max_z_mm);
    if let Some(ref json) = output.stats.objects_info_json {
        let _ = writeln!(s, "objects_info={json}");
    }
    let _ = writeln!(
        s,
        "filament used [mm]={:.2}",
        output.stats.filament_length_mm
    );
    let _ = writeln!(s, "filament used [g]={:.2}", output.stats.filament_weight_g);
    let _ = writeln!(
        s,
        "filament used [cm3]={:.2}",
        output.stats.filament_volume_mm3 * 0.001
    );
    let _ = writeln!(
        s,
        "estimated printing time (normal mode)={}",
        crate::format_duration_hms(output.stats.estimated_time_s)
    );
    let _ = writeln!(
        s,
        "estimated printing time (silent mode)={}",
        crate::format_duration_hms(crate::estimate_silent_time_s(output.stats.estimated_time_s))
    );
    let bw = crate::format_compact_f64(config.machine.bed_width_mm);
    let bd = crate::format_compact_f64(config.machine.bed_depth_mm);
    let _ = writeln!(s, "bed_shape=0x0,{bw}x0,{bw}x{bd},0x{bd}");
    let flavor_name = match config.machine.flavor {
        GcodeFlavor::Klipper => "klipper",
        GcodeFlavor::Marlin => "marlin",
        GcodeFlavor::MarlinBuddy => "marlin2",
    };
    let _ = writeln!(s, "gcode_flavor={flavor_name}");
    s
}

fn build_print_ini(output: &GcodeOutput, config: &PrintConfig) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "filament used [mm]={:.2}",
        output.stats.filament_length_mm
    );
    let _ = writeln!(
        s,
        "filament used [cm3]={:.2}",
        output.stats.filament_volume_mm3 * 0.001
    );
    let _ = writeln!(s, "filament used [g]={:.2}", output.stats.filament_weight_g);
    let _ = writeln!(
        s,
        "total filament used [g]={:.2}",
        output.stats.filament_weight_g
    );
    let _ = writeln!(
        s,
        "estimated printing time (normal mode)={}",
        crate::format_duration_hms(output.stats.estimated_time_s)
    );
    let _ = writeln!(
        s,
        "estimated first layer printing time (normal mode)={}",
        crate::format_duration_hms(output.stats.first_layer_time_s)
    );
    let _ = writeln!(
        s,
        "estimated printing time (silent mode)={}",
        crate::format_duration_hms(crate::estimate_silent_time_s(output.stats.estimated_time_s))
    );
    let _ = writeln!(
        s,
        "estimated first layer printing time (silent mode)={}",
        crate::format_duration_hms(crate::estimate_silent_time_s(
            output.stats.first_layer_time_s
        ))
    );
    if let Some(ft) = config
        .filament
        .filament_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let _ = writeln!(s, "filament_type={ft}");
    }
    let _ = writeln!(
        s,
        "layer_height={}",
        crate::format_compact_f64(config.process.layer_height_mm)
    );
    let _ = writeln!(
        s,
        "first_layer_height={}",
        crate::format_compact_f64(config.process.first_layer_height_mm)
    );
    let _ = writeln!(s, "temperature={}", config.filament.nozzle_temp_c);
    let _ = writeln!(s, "bed_temperature={}", config.filament.bed_temp_c);
    let _ = writeln!(
        s,
        "fill_density={:.0}%",
        config.process.sparse_infill_density * 100.0
    );
    s
}

fn build_slicer_ini(config: &PrintConfig) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "slicer=LayerCraft");
    let _ = writeln!(s, "slicer_version=0.1.0");
    let _ = writeln!(s, "wall_loops={}", config.process.wall_loops);
    let _ = writeln!(
        s,
        "use_relative_e_distances={}",
        u8::from(config.machine.relative_extrusion)
    );
    let _ = writeln!(s, "filament_diameter={:.3}", config.filament.diameter_mm);
    let _ = writeln!(s, "filament_density={:.3}", config.filament.density_g_cm3);
    let _ = writeln!(
        s,
        "nozzle_high_flow={}",
        u8::from(config.machine.nozzle_high_flow)
    );
    let _ = writeln!(
        s,
        "brim_width={}",
        crate::format_compact_f64(config.process.brim_width_mm)
    );
    let _ = writeln!(
        s,
        "support_material={}",
        u8::from(config.process.support.enabled)
    );
    let _ = writeln!(
        s,
        "ironing={}",
        u8::from(config.process.ironing.mode != layercraft_config::IroningType::Off)
    );
    s
}

fn parse_ini_into(text: &str, map: &mut BTreeMap<String, String>) {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim();
            if !key.is_empty() {
                map.insert(key.to_owned(), v.trim().to_owned());
            }
        }
    }
}

fn split_gcode_chunks(gcode: &str, max_bytes: usize) -> Vec<&str> {
    if gcode.is_empty() {
        return Vec::new();
    }
    let bytes = gcode.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let remaining = bytes.len() - start;
        if remaining <= max_bytes {
            if let Some(slice) = gcode.get(start..) {
                chunks.push(slice);
            }
            break;
        }
        let window_end = start + max_bytes;
        let window = bytes.get(start..window_end).unwrap_or(&[]);
        let split_rel = window
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|p| p + 1)
            .filter(|&p| p > 0)
            .unwrap_or_else(|| {
                // Ensure UTF-8 boundary when breaking mid-line on huge lines
                let mut end = window_end;
                while end > start && !gcode.is_char_boundary(end) {
                    end -= 1;
                }
                end.saturating_sub(start).max(1)
            });
        let end = start + split_rel;
        if let Some(slice) = gcode.get(start..end) {
            chunks.push(slice);
        }
        start = end;
    }
    chunks
}

fn extract_gcode_thumbnails(gcode: &str) -> (String, Vec<BgcodeThumbnail>) {
    struct PendingThumb {
        end_marker: &'static str,
        format: BgcodeThumbnailFormat,
        width: u16,
        height: u16,
        b64: String,
    }

    let mut cleaned = String::with_capacity(gcode.len());
    let mut thumbs = Vec::new();
    let mut pending: Option<PendingThumb> = None;

    for segment in gcode.split_inclusive('\n') {
        let trimmed = segment.trim();
        if let Some(mut cur) = pending.take() {
            if trimmed == cur.end_marker {
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&cur.b64)
                    && !decoded.is_empty()
                {
                    thumbs.push(BgcodeThumbnail {
                        format: cur.format,
                        width: cur.width,
                        height: cur.height,
                        data: decoded,
                    });
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix(';') {
                for ch in rest.chars() {
                    if !ch.is_ascii_whitespace() {
                        cur.b64.push(ch);
                    }
                }
                pending = Some(cur);
                continue;
            }
            // Malformed unclosed thumbnail block: drop partial state and keep line
        }

        if let Some((format, end_marker, width, height)) = parse_thumb_begin(trimmed) {
            pending = Some(PendingThumb {
                end_marker,
                format,
                width,
                height,
                b64: String::new(),
            });
            continue;
        }

        cleaned.push_str(segment);
    }

    (cleaned, thumbs)
}

fn parse_thumb_begin(trimmed: &str) -> Option<(BgcodeThumbnailFormat, &'static str, u16, u16)> {
    let (format, rest, end_marker) = if let Some(r) = trimmed.strip_prefix("; thumbnail_QOI begin ")
    {
        (BgcodeThumbnailFormat::Qoi, r, "; thumbnail_QOI end")
    } else if let Some(r) = trimmed.strip_prefix("; thumbnail_JPG begin ") {
        (BgcodeThumbnailFormat::Jpg, r, "; thumbnail_JPG end")
    } else if let Some(r) = trimmed.strip_prefix("; thumbnail_PNG begin ") {
        (BgcodeThumbnailFormat::Png, r, "; thumbnail_PNG end")
    } else {
        let r = trimmed.strip_prefix("; thumbnail begin ")?;
        (BgcodeThumbnailFormat::Png, r, "; thumbnail end")
    };

    let dims = rest.split_whitespace().next()?;
    let (w_str, h_str) = dims.split_once('x')?;
    let w = w_str.parse::<u16>().ok().filter(|&v| v > 0)?;
    let h = h_str.parse::<u16>().ok().filter(|&v| v > 0)?;
    Some((format, end_marker, w, h))
}

fn compress_payload(compression: BgcodeCompression, input: &[u8]) -> Result<Vec<u8>, BgcodeError> {
    match compression {
        BgcodeCompression::None => Ok(input.to_vec()),
        BgcodeCompression::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::new(), FlateLevel::default());
            encoder
                .write_all(input)
                .map_err(|_| BgcodeError::CompressionFailed("zlib write"))?;
            encoder
                .finish()
                .map_err(|_| BgcodeError::CompressionFailed("zlib finish"))
        }
        BgcodeCompression::Heatshrink11_4 => Ok(heatshrink_encode(input, 11, 4)),
        BgcodeCompression::Heatshrink12_4 => Ok(heatshrink_encode(input, 12, 4)),
    }
}

fn decompress_payload(
    compression: BgcodeCompression,
    input: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, BgcodeError> {
    match compression {
        BgcodeCompression::None => {
            if input.len() != expected_len {
                return Err(BgcodeError::DecompressionFailed("raw length mismatch"));
            }
            Ok(input.to_vec())
        }
        BgcodeCompression::Deflate => {
            let mut decoder = ZlibDecoder::new(input);
            let mut out = Vec::with_capacity(expected_len.min(1024 * 1024));
            let mut chunk = [0u8; 8192];
            loop {
                let n = decoder
                    .read(&mut chunk)
                    .map_err(|_| BgcodeError::DecompressionFailed("zlib inflate"))?;
                if n == 0 {
                    break;
                }
                if out.len() + n > expected_len {
                    return Err(BgcodeError::DecompressionFailed(
                        "zlib output exceeds declared size",
                    ));
                }
                if let Some(src) = chunk.get(..n) {
                    out.extend_from_slice(src);
                }
            }
            if out.len() != expected_len {
                return Err(BgcodeError::DecompressionFailed(
                    "zlib output size mismatch",
                ));
            }
            Ok(out)
        }
        BgcodeCompression::Heatshrink11_4 => heatshrink_decode(input, expected_len, 11, 4),
        BgcodeCompression::Heatshrink12_4 => heatshrink_decode(input, expected_len, 12, 4),
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    bit_count: u8,
}

impl BitWriter {
    fn with_capacity(bytes_cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes_cap),
            cur: 0,
            bit_count: 0,
        }
    }

    #[inline]
    fn push_bit(&mut self, bit: bool) {
        if bit {
            self.cur |= 1u8 << (7 - self.bit_count);
        }
        self.bit_count += 1;
        if self.bit_count == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.bit_count = 0;
        }
    }

    #[inline]
    fn push_bits(&mut self, val: u32, width: u8) {
        for shift in (0..width).rev() {
            self.push_bit(((val >> shift) & 1) != 0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
        }
    }

    #[inline]
    fn read_bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.byte_idx)?;
        let bit = ((byte >> (7 - self.bit_idx)) & 1) != 0;
        self.bit_idx += 1;
        if self.bit_idx == 8 {
            self.bit_idx = 0;
            self.byte_idx += 1;
        }
        Some(bit)
    }

    #[inline]
    fn read_bits(&mut self, width: u8) -> Option<u32> {
        let mut out = 0u32;
        for _ in 0..width {
            let bit = self.read_bit()?;
            out = (out << 1) | u32::from(bit);
        }
        Some(out)
    }
}

#[inline]
fn hash_pair(b0: u8, b1: u8) -> usize {
    (((b0 as usize).wrapping_mul(251)) ^ (b1 as usize)) & 4095
}

#[inline]
fn insert_hash_pos(
    pos: usize,
    input: &[u8],
    head: &mut [i32],
    prev: &mut [i32],
    window_mask: usize,
) {
    let Some(&b0) = input.get(pos) else {
        return;
    };
    let Some(&b1) = input.get(pos + 1) else {
        return;
    };
    let h = hash_pair(b0, b1);
    if let (Some(h_slot), Some(p_slot)) = (head.get_mut(h), prev.get_mut(pos & window_mask)) {
        *p_slot = *h_slot;
        *h_slot = pos as i32;
    }
}

/// Clean-room MSB-first Heatshrink LZSS encoder (`window_bits` = 11|12, `lookahead_bits` = 4).
pub(crate) fn heatshrink_encode(input: &[u8], window_bits: u8, lookahead_bits: u8) -> Vec<u8> {
    const HASH_BUCKETS: usize = 4096;
    const MAX_CHAIN: usize = 64;

    let window_size = 1usize << window_bits;
    let window_mask = window_size - 1;
    let max_match_len = 1usize << lookahead_bits;

    let mut head = vec![-1i32; HASH_BUCKETS];
    let mut prev = vec![-1i32; window_size];
    let mut writer = BitWriter::with_capacity(input.len());

    let mut pos = 0usize;
    while pos < input.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;

        if pos + 1 < input.len()
            && let (Some(&b0), Some(&b1)) = (input.get(pos), input.get(pos + 1))
        {
            let h = hash_pair(b0, b1);
            let mut cand = head.get(h).copied().unwrap_or(-1);
            let mut steps = 0usize;

            while cand >= 0 && steps < MAX_CHAIN {
                let c_idx = cand as usize;
                let dist = pos.saturating_sub(c_idx);
                if dist == 0 || dist > window_size {
                    break;
                }
                // Non-overlapping LZSS match (`len <= dist`) guarantees compatibility with all decoders.
                let avail_limit = (input.len() - pos).min(max_match_len).min(dist);
                if avail_limit > best_len {
                    let mut len = 0usize;
                    while len < avail_limit && input.get(c_idx + len) == input.get(pos + len) {
                        len += 1;
                    }
                    if len > best_len {
                        best_len = len;
                        best_dist = dist;
                        if best_len == max_match_len {
                            break;
                        }
                    }
                }
                let next_cand = prev.get(c_idx & window_mask).copied().unwrap_or(-1);
                if next_cand >= cand {
                    break;
                }
                cand = next_cand;
                steps += 1;
            }
        }

        if best_len >= 2 {
            writer.push_bit(false);
            writer.push_bits((best_dist - 1) as u32, window_bits);
            writer.push_bits((best_len - 1) as u32, lookahead_bits);
            for offset in 0..best_len {
                insert_hash_pos(pos + offset, input, &mut head, &mut prev, window_mask);
            }
            pos += best_len;
        } else {
            let byte = input.get(pos).copied().unwrap_or(0);
            writer.push_bit(true);
            writer.push_bits(u32::from(byte), 8);
            insert_hash_pos(pos, input, &mut head, &mut prev, window_mask);
            pos += 1;
        }
    }

    writer.finish()
}

/// Clean-room MSB-first Heatshrink LZSS decoder (`window_bits` = 11|12, `lookahead_bits` = 4).
pub(crate) fn heatshrink_decode(
    input: &[u8],
    expected_len: usize,
    window_bits: u8,
    lookahead_bits: u8,
) -> Result<Vec<u8>, BgcodeError> {
    if expected_len == 0 {
        return Ok(Vec::new());
    }

    let mut reader = BitReader::new(input);
    let mut out = Vec::with_capacity(expected_len.min(1024 * 1024));

    while out.len() < expected_len {
        let is_literal = reader.read_bit().ok_or(BgcodeError::DecompressionFailed(
            "heatshrink unexpected EOF",
        ))?;
        if is_literal {
            let byte = reader.read_bits(8).ok_or(BgcodeError::DecompressionFailed(
                "heatshrink truncated literal",
            ))? as u8;
            out.push(byte);
        } else {
            let dist_minus_1 =
                reader
                    .read_bits(window_bits)
                    .ok_or(BgcodeError::DecompressionFailed(
                        "heatshrink truncated backreference index",
                    ))? as usize;
            let len_minus_1 =
                reader
                    .read_bits(lookahead_bits)
                    .ok_or(BgcodeError::DecompressionFailed(
                        "heatshrink truncated backreference length",
                    ))? as usize;
            let dist = dist_minus_1 + 1;
            let len = len_minus_1 + 1;
            if dist > out.len() || out.len() + len > expected_len {
                return Err(BgcodeError::DecompressionFailed(
                    "heatshrink invalid backreference bounds",
                ));
            }
            for _ in 0..len {
                let src_idx = out.len() - dist;
                let b = out
                    .get(src_idx)
                    .copied()
                    .ok_or(BgcodeError::DecompressionFailed(
                        "heatshrink backreference out of range",
                    ))?;
                out.push(b);
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{emit_gcode, evaluate_gcode};
    use layercraft_config::ProfileLibrary;
    use layercraft_slice::slice_single_mesh;

    #[test]
    fn heatshrink_known_vector_and_roundtrips() {
        // 'a' (lit), 'b' (lit), backref dist=2, len=2 (window=11, lookahead=4) -> [0xB0, 0xD8, 0x80, 0x04, 0x40]
        let vec_bytes = [0xB0u8, 0xD8, 0x80, 0x04, 0x40];
        let decoded_vec = heatshrink_decode(&vec_bytes, 4, 11, 4).unwrap();
        assert_eq!(decoded_vec, b"abab");

        // Overlapping backref RLE test on decoder: 'A', backref dist=1, len=5 -> "AAAAAA"
        let mut rle_writer = BitWriter::with_capacity(8);
        rle_writer.push_bit(true);
        rle_writer.push_bits(u32::from(b'A'), 8);
        rle_writer.push_bit(false);
        rle_writer.push_bits(0, 11); // dist = 1
        rle_writer.push_bits(4, 4); // len = 5
        let rle_bits = rle_writer.finish();
        assert_eq!(heatshrink_decode(&rle_bits, 6, 11, 4).unwrap(), b"AAAAAA");

        // Round-trip empty, short, repetitive, and pseudo-random payloads on both window sizes
        let mut pseudo_random = Vec::with_capacity(4096);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..4096 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            pseudo_random.push((state >> 24) as u8);
        }
        let repetitive: Vec<u8> = b"G1 X100.123 Y100.456 E1.2345\n"
            .iter()
            .copied()
            .cycle()
            .take(25_000)
            .collect();

        for payload in [
            b"".as_slice(),
            b"Z",
            b"AAAAAA",
            b"G28\nG1 X10 Y20 E0.5\nG1 X20 Y20 E1.0\n",
            pseudo_random.as_slice(),
            repetitive.as_slice(),
        ] {
            for wbits in [11u8, 12u8] {
                let enc = heatshrink_encode(payload, wbits, 4);
                let dec = heatshrink_decode(&enc, payload.len(), wbits, 4).unwrap();
                assert_eq!(
                    dec,
                    payload,
                    "mismatch for len={} wbits={wbits}",
                    payload.len()
                );
            }
        }
    }

    #[test]
    fn bgcode_roundtrips_all_compressions_with_core_one_thumbnails_and_eval() {
        let lib = ProfileLibrary::with_builtin_presets();
        let cfg = lib
            .build_print_config(
                &PrintConfig::default(),
                Some("LayerCraft CORE One+ HF 0.4mm"),
                Some("LayerCraft PLA (0.4mm HF Nozzle)"),
                Some("LayerCraft 0.20mm Balanced (0.4mm HF)"),
            )
            .unwrap()
            .config;

        let mesh = layercraft_testkit::make_box(20.0, 20.0, 6.0).unwrap();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let gcode_out = emit_gcode(&plate, &cfg).unwrap();
        let (ascii_report, _) = evaluate_gcode(
            &gcode_out.gcode,
            cfg.filament.diameter_mm,
            cfg.machine.bed_width_mm,
            cfg.machine.bed_depth_mm,
        )
        .unwrap();

        for compression in [
            BgcodeCompression::None,
            BgcodeCompression::Deflate,
            BgcodeCompression::Heatshrink11_4,
            BgcodeCompression::Heatshrink12_4,
        ] {
            let opts = BgcodeOptions {
                compression,
                metadata_compression: BgcodeCompression::None,
                checksum: BgcodeChecksum::Crc32,
                gcode_chunk_bytes: 4096,
            };
            let bgcode_bytes = encode_bgcode(&gcode_out, &cfg, &opts).unwrap();
            assert!(is_bgcode(&bgcode_bytes));
            assert_eq!(&bgcode_bytes[..4], b"GCDE");

            if compression != BgcodeCompression::None {
                assert!(
                    bgcode_bytes.len() * 100 < gcode_out.gcode.len() * 65,
                    "expected compressed bgcode <65% of ASCII ({:?}: {} vs {})",
                    compression,
                    bgcode_bytes.len(),
                    gcode_out.gcode.len()
                );
            }

            let decoded = decode_bgcode(&bgcode_bytes).unwrap();
            assert_eq!(decoded.version, 1);
            assert_eq!(decoded.checksum, BgcodeChecksum::Crc32);
            assert_eq!(
                decoded.file_metadata.get("Producer").map(String::as_str),
                Some("LayerCraft 0.1.0")
            );
            assert_eq!(
                decoded
                    .printer_metadata
                    .get("printer_model")
                    .map(String::as_str),
                Some("COREONE")
            );
            assert_eq!(
                decoded
                    .printer_metadata
                    .get("filament_type")
                    .map(String::as_str),
                Some("PLA")
            );
            assert_eq!(
                decoded
                    .printer_metadata
                    .get("nozzle_diameter")
                    .map(String::as_str),
                Some("0.4")
            );
            assert_eq!(
                decoded
                    .printer_metadata
                    .get("nozzle_high_flow")
                    .map(String::as_str),
                Some("1")
            );
            assert!(
                decoded
                    .printer_metadata
                    .get("estimated printing time (normal mode)")
                    .is_some_and(|s| !s.is_empty())
            );
            assert!(
                decoded
                    .printer_metadata
                    .get("estimated printing time (silent mode)")
                    .is_some_and(|s| !s.is_empty())
            );
            assert!(decoded.printer_metadata.contains_key("filament used [g]"));
            assert!(decoded.printer_metadata.contains_key("max_layer_z"));
            assert!(
                decoded
                    .print_metadata
                    .contains_key("total filament used [g]")
            );
            assert!(
                decoded
                    .print_metadata
                    .contains_key("estimated first layer printing time (normal mode)")
            );
            assert_eq!(decoded.thumbnails.len(), 4);
            assert_eq!(decoded.thumbnails[0].format, BgcodeThumbnailFormat::Qoi);
            assert_eq!(&decoded.thumbnails[0].data[..4], b"qoif");
            assert_eq!(decoded.thumbnails[1].format, BgcodeThumbnailFormat::Qoi);
            assert_eq!(&decoded.thumbnails[1].data[..4], b"qoif");
            assert_eq!(decoded.thumbnails[2].format, BgcodeThumbnailFormat::Qoi);
            assert_eq!(&decoded.thumbnails[2].data[..4], b"qoif");
            assert_eq!(decoded.thumbnails[3].format, BgcodeThumbnailFormat::Png);
            assert_eq!(&decoded.thumbnails[3].data[..8], b"\x89PNG\r\n\x1a\n");

            let (decoded_report, _) = evaluate_gcode(
                &decoded.gcode,
                cfg.filament.diameter_mm,
                cfg.machine.bed_width_mm,
                cfg.machine.bed_depth_mm,
            )
            .unwrap();
            assert!(
                decoded_report.safety.is_clean(),
                "{:?}",
                decoded_report.safety
            );
            assert!(
                (decoded_report.total_filament_volume_mm3 - ascii_report.total_filament_volume_mm3)
                    .abs()
                    < 1e-3
            );
            assert_eq!(
                decoded_report.active_layer_count,
                ascii_report.active_layer_count
            );
        }
    }

    #[test]
    fn bgcode_crc_mismatch_and_hostile_inputs_fail_cleanly() {
        let cfg = PrintConfig::default();
        let mesh = layercraft_testkit::make_box(12.0, 12.0, 2.0).unwrap();
        let plate = slice_single_mesh(&mesh, &cfg).unwrap();
        let gcode_out = emit_gcode(&plate, &cfg).unwrap();

        let mut encoded = encode_bgcode(&gcode_out, &cfg, &BgcodeOptions::default()).unwrap();
        let last_idx = encoded.len() - 1;
        encoded[last_idx] ^= 0x01;
        assert!(matches!(
            decode_bgcode(&encoded),
            Err(BgcodeError::ChecksumMismatch { .. })
        ));

        for hostile in [
            b"".as_slice(),
            b"GCD",
            b"NOPE123456",
            b"GCDE\x02\x00\x00\x00\x00\x00",
            b"GCDE\x01\x00\x00\x00\x05\x00",
            b"GCDE\x01\x00\x00\x00\x01\x00\x01\x00",
            b"GCDE\x01\x00\x00\x00\x00\x00\x01\x00\x02\x00\xff\xff\xff\x7f\x04\x00\x00\x00",
        ] {
            assert!(decode_bgcode(hostile).is_err());
        }
    }
}
