use std::path::Path;

fn main() {
    // Convert icon-512.png to icon.ico for embedding in the exe
    let png_path = Path::new("icon-512.png");
    let ico_path = Path::new("icon.ico");

    if png_path.exists() && !ico_path.exists() {
        let img = image::open(png_path).expect("Failed to open icon-512.png");

        // ICO needs specific sizes — 256x256 is the max, plus smaller for taskbar/alt-tab
        let sizes = [256, 48, 32, 16];
        let mut ico_buf: Vec<u8> = Vec::new();

        // Write ICO header
        let count = sizes.len() as u16;
        ico_buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        ico_buf.extend_from_slice(&1u16.to_le_bytes()); // type: icon
        ico_buf.extend_from_slice(&count.to_le_bytes()); // image count

        // We'll fill in the directory entries after encoding each image
        let dir_start = ico_buf.len();
        // Reserve space for directory entries (16 bytes each)
        ico_buf.resize(dir_start + (count as usize) * 16, 0);

        let mut data_offset = ico_buf.len() as u32;

        for (i, &size) in sizes.iter().enumerate() {
            let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
            let rgba = resized.to_rgba8();

            // Encode as PNG for the ICO entry (modern ICO supports embedded PNG)
            let mut png_data = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
            image::ImageEncoder::write_image(
                encoder,
                rgba.as_raw(),
                size,
                size,
                image::ColorType::Rgba8,
            )
            .expect("Failed to encode PNG for ICO");

            let data_size = png_data.len() as u32;

            // Write directory entry
            let entry_offset = dir_start + i * 16;
            let w = if size >= 256 { 0u8 } else { size as u8 };
            let h = w;
            ico_buf[entry_offset] = w;
            ico_buf[entry_offset + 1] = h;
            ico_buf[entry_offset + 2] = 0; // color palette
            ico_buf[entry_offset + 3] = 0; // reserved
            ico_buf[entry_offset + 4..entry_offset + 6].copy_from_slice(&1u16.to_le_bytes()); // color planes
            ico_buf[entry_offset + 6..entry_offset + 8].copy_from_slice(&32u16.to_le_bytes()); // bits per pixel
            ico_buf[entry_offset + 8..entry_offset + 12].copy_from_slice(&data_size.to_le_bytes());
            ico_buf[entry_offset + 12..entry_offset + 16].copy_from_slice(&data_offset.to_le_bytes());

            ico_buf.extend_from_slice(&png_data);
            data_offset += data_size;
        }

        std::fs::write(ico_path, &ico_buf).expect("Failed to write icon.ico");
        println!("cargo:warning=Generated icon.ico from icon-512.png");
    }

    // Embed the icon in the Windows exe
    if cfg!(target_os = "windows") && ico_path.exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().expect("Failed to compile Windows resources");
    }

    println!("cargo:rerun-if-changed=icon-512.png");
    println!("cargo:rerun-if-changed=icon.ico");
}
