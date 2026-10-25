//! The local clipboard, with any data type. On macOS this uses NSPasteboard
//! directly; elsewhere it calls wl-copy/wl-paste or xclip.

use anyhow::{Result, bail};

/// Map common X11/Wayland names to one MIME type.
pub fn normalize(mime: &str) -> String {
    let m = mime.trim();
    match m.to_ascii_lowercase().as_str() {
        "" | "text" | "utf8_string" | "string" | "text/plain" | "text/plain;charset=utf-8" | "text/plain;charset=utf8"
        | "compound_text" => "text/plain".to_string(),
        "image" => "image/png".to_string(),
        _ => m.to_string(),
    }
}

pub fn is_text(mime: &str) -> bool {
    normalize(mime).starts_with("text/")
}

/// MIME types offered for a set of macOS pasteboard types (UTIs).
pub fn mimes_from_utis(utis: &[String], file_image: bool) -> Vec<String> {
    let has = |u: &str| utis.iter().any(|x| x == u);
    let mut v: Vec<String> = Vec::new();
    let mut push = |m: &str| {
        if !v.iter().any(|x| x == m) {
            v.push(m.to_string());
        }
    };
    if file_image || has("public.png") || has("public.tiff") || has("public.jpeg") {
        push("image/png");
    }
    if has("public.tiff") {
        push("image/tiff");
    }
    if has("public.jpeg") {
        push("image/jpeg");
    }
    if has("com.compuserve.gif") {
        push("image/gif");
    }
    if has("public.html") {
        push("text/html");
    }
    if has("public.rtf") {
        push("text/rtf");
    }
    if has("public.file-url") {
        push("text/uri-list");
    }
    if has("public.utf8-plain-text") || has("NSStringPboardType") {
        for m in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"] {
            push(m);
        }
    }
    push("TARGETS");
    v
}

pub fn uti_for(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "text/plain" => "public.utf8-plain-text",
        "text/html" => "public.html",
        "text/rtf" | "application/rtf" => "public.rtf",
        "image/png" => "public.png",
        "image/tiff" => "public.tiff",
        "image/jpeg" | "image/jpg" => "public.jpeg",
        "image/gif" => "com.compuserve.gif",
        "text/uri-list" => "public.file-url",
        "application/pdf" => "com.adobe.pdf",
        _ => return None,
    })
}

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "heic", "tif", "tiff", "bmp"];

pub fn is_image_path(p: &str) -> bool {
    p.rsplit('.').next().is_some_and(|e| IMAGE_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use objc2::{rc::autoreleasepool, runtime::AnyObject};
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
        NSPasteboardTypeString, NSPasteboardTypeTIFF,
    };
    use objc2_foundation::{NSData, NSDictionary, NSString, NSURL};

    fn file_url_path(pb: &NSPasteboard) -> Option<String> {
        let s = pb.stringForType(unsafe { NSPasteboardTypeFileURL })?;
        let url = NSURL::URLWithString(&s)?;
        if !url.isFileURL() {
            return None;
        }
        Some(url.path()?.to_string())
    }

    fn to_png(data: &[u8]) -> Option<Vec<u8>> {
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(data.to_vec());
        }
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(data))?;
        let props = NSDictionary::<NSString, AnyObject>::new();
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
        Some(png.to_vec())
    }

    pub fn list() -> Result<Vec<String>> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let utis: Vec<String> = pb.types().map(|a| a.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
            let file_image = file_url_path(&pb).is_some_and(|p| is_image_path(&p));
            Ok(mimes_from_utis(&utis, file_image))
        })
    }

    pub fn get(mime: &str) -> Result<(String, Vec<u8>)> {
        let m = normalize(mime);
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            match m.as_str() {
                "text/plain" => match pb.stringForType(unsafe { NSPasteboardTypeString }) {
                    Some(s) => Ok((m.clone(), s.to_string().into_bytes())),
                    None => bail!("the clipboard has no text"),
                },
                "image/png" => {
                    // A copied image file (Finder) comes first: the pasteboard's own
                    // image is then only the file icon.
                    if let Some(p) = file_url_path(&pb).filter(|p| is_image_path(p)) {
                        if let Some(png) = std::fs::read(&p).ok().and_then(|d| to_png(&d)) {
                            return Ok((m.clone(), png));
                        }
                    }
                    if let Some(d) = pb.dataForType(unsafe { NSPasteboardTypePNG }) {
                        return Ok((m.clone(), d.to_vec()));
                    }
                    if let Some(png) = pb.dataForType(unsafe { NSPasteboardTypeTIFF }).and_then(|d| to_png(&d.to_vec())) {
                        return Ok((m.clone(), png));
                    }
                    bail!("the clipboard has no image")
                }
                "text/uri-list" => match pb.stringForType(unsafe { NSPasteboardTypeFileURL }) {
                    Some(s) => Ok((m.clone(), format!("{s}\r\n").into_bytes())),
                    None => bail!("the clipboard has no file"),
                },
                other => {
                    let uti = uti_for(other).map(str::to_string).unwrap_or_else(|| other.to_string());
                    match pb.dataForType(&NSString::from_str(&uti)) {
                        Some(d) => Ok((m.clone(), d.to_vec())),
                        None => bail!("the clipboard has no {other}"),
                    }
                }
            }
        })
    }

    pub fn set(mime: &str, data: &[u8]) -> Result<()> {
        let m = normalize(mime);
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ok = match m.as_str() {
                "text/plain" => {
                    let s = String::from_utf8_lossy(data);
                    pb.setString_forType(&NSString::from_str(&s), unsafe { NSPasteboardTypeString })
                }
                "image/png" => {
                    let d = NSData::with_bytes(data);
                    let ok = pb.setData_forType(Some(&d), unsafe { NSPasteboardTypePNG });
                    if let Some(t) = NSBitmapImageRep::imageRepWithData(&d).and_then(|r| r.TIFFRepresentation()) {
                        pb.setData_forType(Some(&t), unsafe { NSPasteboardTypeTIFF });
                    }
                    ok
                }
                other => {
                    let uti = uti_for(other).map(str::to_string).unwrap_or_else(|| other.to_string());
                    let ok = pb.setData_forType(Some(&NSData::with_bytes(data)), &NSString::from_str(&uti));
                    if other.starts_with("text/") {
                        if let Ok(s) = std::str::from_utf8(data) {
                            pb.setString_forType(&NSString::from_str(s), unsafe { NSPasteboardTypeString });
                        }
                    }
                    ok
                }
            };
            if !ok {
                bail!("cannot set the clipboard");
            }
            Ok(())
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    fn wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    fn run(cmd: &mut Command) -> Result<Vec<u8>> {
        let out = cmd.stderr(Stdio::null()).output()?;
        if !out.status.success() {
            bail!("clipboard tool failed");
        }
        Ok(out.stdout)
    }

    pub fn list() -> Result<Vec<String>> {
        let out = if wayland() {
            run(Command::new("wl-paste").arg("-l"))?
        } else {
            run(Command::new("xclip").args(["-selection", "clipboard", "-t", "TARGETS", "-o"]))?
        };
        Ok(String::from_utf8_lossy(&out).lines().map(str::to_string).collect())
    }

    pub fn get(mime: &str) -> Result<(String, Vec<u8>)> {
        let m = normalize(mime);
        let out = if wayland() {
            run(Command::new("wl-paste").args(["-n", "-t", &m]))?
        } else {
            run(Command::new("xclip").args(["-selection", "clipboard", "-t", &m, "-o"]))?
        };
        Ok((m, out))
    }

    pub fn set(mime: &str, data: &[u8]) -> Result<()> {
        let m = normalize(mime);
        let mut child = if wayland() {
            Command::new("wl-copy").args(["-t", &m]).stdin(Stdio::piped()).spawn()?
        } else {
            Command::new("xclip").args(["-selection", "clipboard", "-t", &m, "-i"]).stdin(Stdio::piped()).spawn()?
        };
        child.stdin.take().unwrap().write_all(data)?;
        child.wait()?;
        Ok(())
    }
}

pub use imp::{get, list, set};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_mapping() {
        assert_eq!(normalize("UTF8_STRING"), "text/plain");
        assert_eq!(normalize("text/plain;charset=utf-8"), "text/plain");
        let v = mimes_from_utis(&["public.png".into(), "public.utf8-plain-text".into()], false);
        assert_eq!(v[0], "image/png");
        assert!(v.contains(&"UTF8_STRING".to_string()));
        assert!(is_image_path("/a/b/Shot 1.PNG"));
    }
}
