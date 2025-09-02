use image::{GenericImageView, ImageBuffer, RgbaImage};
use itertools::Itertools;
use windows::core::PCWSTR;
use windows::Win32::{
    Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS,
    },
    Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
    UI::{
        Controls::{IImageList, ILD_TRANSPARENT},
        Shell::{SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SYSICONINDEX, SHIL_JUMBO},
        WindowsAndMessaging::{DestroyIcon, GetIconInfoExW, HICON, ICONINFOEXW},
    },
};

use seelen_core::state::Icon;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    __m128i, _mm_loadu_si128, _mm_setr_epi8, _mm_shuffle_epi8, _mm_storeu_si128,
};

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::{
    uint8x16_t, vld1q_u8, vqtbl1q_u8, vst1q_u8
};

use std::io::BufRead;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error_handler::Result;
use crate::modules::start::application::START_MENU_MANAGER;
use crate::modules::uwp::UwpManager;
use crate::state::application::FULL_STATE;
use crate::trace_lock;
use crate::utils::constants::SEELEN_COMMON;
use crate::windows_api::types::AppUserModelId;
use crate::windows_api::WindowsApi;

/// Convert BGRA to RGBA
///
/// Uses SIMD to go fast
#[cfg(target_arch = "x86_64")]
pub fn bgra_to_rgba(data: &mut [u8]) {
    // The shuffle mask for converting BGRA -> RGBA
    let mask: __m128i = unsafe {
        _mm_setr_epi8(
            2, 1, 0, 3, // First pixel
            6, 5, 4, 7, // Second pixel
            10, 9, 8, 11, // Third pixel
            14, 13, 12, 15, // Fourth pixel
        )
    };
    // For each 16-byte chunk in your data
    for chunk in data.chunks_exact_mut(16) {
        let mut vector = unsafe { _mm_loadu_si128(chunk.as_ptr() as *const __m128i) };
        vector = unsafe { _mm_shuffle_epi8(vector, mask) };
        unsafe { _mm_storeu_si128(chunk.as_mut_ptr() as *mut __m128i, vector) };
    }
}

// Uses NEON intrinsics to go fast
#[cfg(target_arch = "aarch64")]
pub fn bgra_to_rgba(data: &mut [u8]) {
    // The shuffle mask for converting BGRA -> RGBA
    let maskplain: [u8; 16] = [
        2, 1, 0, 3, // First pixel
        6, 5, 4, 7, // Second pixel
        10, 9, 8, 11, // Third pixel
        14, 13, 12, 15 // Fourth pixel
    ];
    // The shuffle mask for the conversion in NEON intrinsics
    let mask: uint8x16_t = unsafe { vld1q_u8(maskplain.as_ptr()) };
    // For each 16-byte chunk in your data
    for chunk in data.chunks_exact_mut(16) {
        let mut vector: uint8x16_t = unsafe { vld1q_u8(chunk.as_ptr()) };
        vector = unsafe { vqtbl1q_u8(vector, mask) };
        unsafe { vst1q_u8(chunk.as_mut_ptr(), vector) };
    }
}

pub fn convert_hicon_to_rgba_image(hicon: &HICON) -> Result<RgbaImage> {
    unsafe {
        let mut icon_info = ICONINFOEXW {
            cbSize: std::mem::size_of::<ICONINFOEXW>() as u32,
            ..Default::default()
        };

        if !GetIconInfoExW(*hicon, &mut icon_info).as_bool() {
            return Err("Failed to get icon info".into());
        }
        let hdc_screen = CreateCompatibleDC(None);
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        let hbm_old = SelectObject(hdc_mem, icon_info.hbmColor.into());

        let mut bmp_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: icon_info.xHotspot as i32 * 2,
                biHeight: -(icon_info.yHotspot as i32 * 2),
                biPlanes: 1,
                biBitCount: 32, // 4 bytes per pixel
                biCompression: DIB_RGB_COLORS.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut buffer: Vec<u8> =
            vec![0; (icon_info.xHotspot * 2 * icon_info.yHotspot * 2 * 4) as usize];

        if GetDIBits(
            hdc_mem,
            icon_info.hbmColor,
            0,
            icon_info.yHotspot * 2,
            Some(buffer.as_mut_ptr() as *mut _),
            &mut bmp_info,
            DIB_RGB_COLORS,
        ) == 0
        {
            return Err("Failed to get dibits".into());
        }

        // Clean up
        SelectObject(hdc_mem, hbm_old);
        DeleteDC(hdc_mem).ok()?;
        DeleteDC(hdc_screen).ok()?;
        DeleteObject(icon_info.hbmColor.into()).ok()?;
        DeleteObject(icon_info.hbmMask.into()).ok()?;

        if bmp_info.bmiHeader.biBitCount != 32 {
            return Err("Icon is not 32 bit".into());
        }

        bgra_to_rgba(buffer.as_mut_slice());

        let image = ImageBuffer::from_raw(icon_info.xHotspot * 2, icon_info.yHotspot * 2, buffer)
            .expect("Failed to create image buffer");
        Ok(image)
    }
}

/// this is the best solution having in consideration that a transparent image and have separated pixels
/// with transparent gaps, so search side by side and crop them is the best approach.
pub fn crop_transparent_borders(rgba_image: &RgbaImage) -> RgbaImage {
    let (width, height) = rgba_image.dimensions();
    let mut top = None;
    let mut bottom = None;
    let mut left = None;
    let mut right = None;

    'outer: for y in 0..height {
        for x in 0..width {
            let pixel = rgba_image.get_pixel(x, y);
            if pixel.0[3] != 0 {
                top = Some(y);
                break 'outer;
            }
        }
    }

    let top = match top {
        Some(top) => top,
        None => return RgbaImage::new(1, 1),
    };

    'outer: for y in (top..height).rev() {
        for x in 0..width {
            let pixel = rgba_image.get_pixel(x, y);
            if pixel.0[3] != 0 {
                bottom = Some(y);
                break 'outer;
            }
        }
    }

    let bottom = match bottom {
        Some(bottom) => bottom,
        None => return RgbaImage::new(1, 1),
    };

    'outer: for x in 0..width {
        for y in top..bottom {
            let pixel = rgba_image.get_pixel(x, y);
            if pixel.0[3] != 0 {
                left = Some(x);
                break 'outer;
            }
        }
    }

    let left = match left {
        Some(left) => left,
        None => return RgbaImage::new(1, 1),
    };

    'outer: for x in (left..width).rev() {
        for y in top..bottom {
            let pixel = rgba_image.get_pixel(x, y);
            if pixel.0[3] != 0 {
                right = Some(x);
                break 'outer;
            }
        }
    }

    let right = match right {
        Some(right) => right,
        None => return RgbaImage::new(1, 1),
    };

    rgba_image
        .view(left, top, right - left + 1, bottom - top + 1)
        .to_image()
}

pub fn get_icon_from_file(path: &Path) -> Result<RgbaImage> {
    unsafe {
        let path_str = path.as_os_str().encode_wide().chain(Some(0)).collect_vec();

        let mut file_info = SHFILEINFOW::default();
        let result = SHGetFileInfoW(
            PCWSTR(path_str.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut file_info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );

        // file_info.iIcon = 0 is a valid icon but it is the default icon for files on Windows
        // so we will handle this as no icon to avoid generate unnecessary artifacts
        if result == 0 || file_info.iIcon == 0 {
            return Err("Failed to get icon".into());
        }

        let image_list: IImageList = SHGetImageList(SHIL_JUMBO as i32)?;
        // if 256x256 icon is not available, will use the icons with the most color depth and size
        // this is useful for some icons where color depth is less than 32,
        // example: icon of 124x124 16bits and other 64x64 32bits this will return the 32bits icon
        // color depth is prioritized over size
        let icon = image_list.GetIcon(file_info.iIcon, ILD_TRANSPARENT.0)?;
        let image = crop_transparent_borders(&convert_hicon_to_rgba_image(&icon)?);
        DestroyIcon(icon)?;
        Ok(image)
    }
}

// maintain this function as documentation for url files
#[allow(dead_code)]
fn get_icon_from_url_file(path: &Path) -> Result<RgbaImage> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut path = None;
    // in theory .url files are encoded in UTF-8 so we don't need to use OsString
    for line in reader.lines() {
        if let Some(stripped) = line?.strip_prefix("IconFile=") {
            path = Some(PathBuf::from(stripped));
            break;
        }
    }

    let path = match path {
        Some(icon_file) => icon_file,
        None => return Err("Failed to get icon".into()),
    };

    get_icon_from_file(&path)
}

/// returns the path of the icon extracted from the executable or copied if is an UWP app.
///
/// If the icon already exists, it returns the path instead overriding, this is needed for allow user custom icons.
pub fn extract_and_save_icon_from_file<T: AsRef<Path>>(path: T) -> Result<()> {
    let origin = path.as_ref();
    if !origin.exists() || origin.is_dir() {
        return Err("Path is not a file".into());
    }

    let origin_ext = match origin.extension() {
        Some(ext) => ext.to_string_lossy().to_lowercase(),
        // no extension === no icon
        None => return Ok(()),
    };

    let key = origin.to_string_lossy().to_string();

    let is_exe_file = origin_ext == "exe";
    let is_lnk_file = origin_ext == "lnk";
    let is_url_file = origin_ext == "url";

    let mutex = FULL_STATE.load().icon_packs().clone();
    let mut icon_manager = trace_lock!(mutex);
    if is_exe_file || is_lnk_file {
        if icon_manager.get_app_icon(&key).is_some() {
            return Ok(());
        }
    } else if icon_manager.get_file_icon(origin).is_some() {
        return Ok(());
    }

    let file_name = origin.file_name().ok_or("Failed to get file name")?;

    let to_store_filename = PathBuf::from(format!("{}.png", uuid::Uuid::new_v4()));
    let to_store_path = SEELEN_COMMON
        .user_icons_path()
        .join("system")
        .join(&to_store_filename);

    log::trace!("Extracting icon for {:?}", file_name);

    // Special case for url files, these can have a custom icon, but to simplicity we use a single one for every .url
    if is_url_file {
        let url_placeholder_icon = SEELEN_COMMON.app_resource_dir().join("icons/url.png");
        std::fs::copy(&url_placeholder_icon, &to_store_path)?;
        icon_manager.add_system_file_icon("url", Icon::Static(to_store_filename));
        icon_manager.write_system_icon_pack()?;
        return Ok(());
    }

    // try get the icon directly from the file
    if let Ok(icon) = get_icon_from_file(origin) {
        icon.save(&to_store_path)?;
        if is_exe_file || is_lnk_file {
            icon_manager.add_system_app_icon(&key, Icon::Static(to_store_filename));
        } else {
            icon_manager.add_system_file_icon(&origin_ext, Icon::Static(to_store_filename));
        }
        icon_manager.write_system_icon_pack()?;
        return Ok(());
    }

    // if the lnk don't have an icon, try to extract it from the target
    if is_lnk_file {
        drop(icon_manager);
        let (target, _) = WindowsApi::resolve_lnk_target(origin)?;
        extract_and_save_icon_from_file(&target)?;

        let target_key = target.to_string_lossy().to_string();
        let mut icon_manager = trace_lock!(mutex);
        let target_icon = icon_manager.get_app_icon(&target_key).ok_or("Ups")?.clone();
        icon_manager.add_system_app_icon(&key, target_icon);
        icon_manager.write_system_icon_pack()?;
        return Ok(());
    }

    Err("Failed to extract icon".into())
}

/// returns the path of the icon extracted from the app with the specified package app user model id.
pub fn extract_and_save_icon_umid(aumid: &AppUserModelId) -> Result<()> {
    let icon_manager_mutex = FULL_STATE.load().icon_packs().clone();
    {
        let manager = trace_lock!(icon_manager_mutex);
        if manager.get_app_icon(aumid.as_ref()).is_some() {
            return Ok(());
        }
    }

    log::trace!("Extracting icon for {:?}", aumid);
    match aumid {
        AppUserModelId::Appx(aumid) => {
            let (light, dark) = UwpManager::get_high_quality_icon_path(aumid)?;
            let root = SEELEN_COMMON.user_icons_path().join("system");
            let name = uuid::Uuid::new_v4();
            std::fs::copy(light, root.join(&format!("{}_light.png", name)))?;
            std::fs::copy(dark, root.join(&format!("{}_dark.png", name)))?;

            let mut icon_manager = trace_lock!(icon_manager_mutex);
            icon_manager.add_system_app_icon(
                aumid,
                Icon::Dynamic {
                    light: format!("{}_light.png", name).into(),
                    dark: format!("{}_dark.png", name).into(),
                    mask: None,
                },
            );
            icon_manager.write_system_icon_pack()
        }
        AppUserModelId::PropertyStore(aumid) => {
            let lnk = START_MENU_MANAGER
                .load()
                .search_shortcut_with_same_umid(aumid)
                .ok_or("No shortcut found for umid")?;
            extract_and_save_icon_from_file(&lnk)?;

            let mut icon_manager = trace_lock!(icon_manager_mutex);
            let lnk_key = lnk.to_string_lossy().to_string();
            let target_icon = icon_manager.get_app_icon(&lnk_key).ok_or("Ups")?.clone();
            icon_manager.add_system_app_icon(aumid, target_icon);
            icon_manager.write_system_icon_pack()?;
            Ok(())
        }
    }
}
