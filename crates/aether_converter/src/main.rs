use byteorder::{LittleEndian, WriteBytesExt};
use clap::Parser;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long)]
    input: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let file = File::open(&args.input).map_err(|e| format!("Failed to open input: {}", e))?;
    let mut decoder = Decoder::new(file).map_err(|e| format!("Bad TIFF format: {}", e))?;

    let (width, height) = decoder.dimensions().map_err(|e| format!("No dims: {}", e))?;

    // Read Tiepoints/Scale
    let tiepoints = decoder.get_tag_f64_vec(Tag::ModelTiepointTag).unwrap_or_default();
    let pixel_scales = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag).unwrap_or_default();

    let (ul_lon, ul_lat, scale_x, scale_y) = if tiepoints.len() >= 6 && pixel_scales.len() >= 2 {
        (tiepoints[3], tiepoints[4], pixel_scales[0], pixel_scales[1])
    } else {
        (0.0, 0.0, 0.0002777, 0.0002777)
    };

    println!("[Rust] Conv: {:?} [{}x{}] Lat:{:.2} Lon:{:.2}",
             args.input.file_name().unwrap(), width, height, ul_lat, ul_lon);

    let decoding_result = decoder.read_image().map_err(|e| format!("Decode failed: {}", e))?;

    // --- FIX: Handle Float32 inputs ---
    let elevation_data: Vec<i16> = match decoding_result {
        DecodingResult::I16(data) => {
            // Input is Int16 Meters -> Output is Int16 Half-Meters
            data.iter().map(|&x| x.saturating_mul(2)).collect()
        },
        DecodingResult::I32(data) => {
            // Downcast and scale
            data.iter().map(|&x| (x as i16).saturating_mul(2)).collect()
        },
        DecodingResult::F32(data) => {
            // Input is Float32 Meters -> Output is Int16 Half-Meters
            // e.g. 100.5m -> 201
            data.iter().map(|&x| (x * 2.0).round() as i16).collect()
        },
        DecodingResult::F64(data) => {
            data.iter().map(|&x| (x * 2.0).round() as i16).collect()
        },
        _ => return Err("Unsupported TIFF data type (must be Int or Float elevation)".into()),
    };

    // Write ABT
    let out_file = File::create(&args.output)?;
    let mut writer = BufWriter::new(out_file);

    writer.write_all(b"AETH")?;
    writer.write_u16::<LittleEndian>(1)?;
    writer.write_u16::<LittleEndian>(width as u16)?;
    writer.write_f64::<LittleEndian>(ul_lat)?;
    writer.write_f64::<LittleEndian>(ul_lon)?;
    writer.write_f64::<LittleEndian>(scale_y)?;
    writer.write_f64::<LittleEndian>(scale_x)?;
    writer.write_i16::<LittleEndian>(0)?; // BaseElev
    writer.write_u16::<LittleEndian>(0)?; // Padding

    for &h in &elevation_data {
        writer.write_i16::<LittleEndian>(h)?;
    }

    Ok(())
}