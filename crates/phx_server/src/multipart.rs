//! `multipart/form-data` parsing for form uploads.

use phx_vm::UploadedFile;

/// At most this many parts are accepted in one request.
const MAX_PARTS: usize = 1000;

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// The `boundary` parameter of a multipart Content-Type.
pub fn boundary(content_type: &str) -> Option<String> {
    let mut parts = content_type.split(';');
    if !parts.next()?.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    parts
        .find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
        })
        .filter(|b| !b.is_empty() && b.len() <= 200)
}

/// A parameter (`name="x"`) from a Content-Disposition header.
fn param(header: &str, key: &str) -> Option<String> {
    header.split(';').skip(1).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Keeps only the last path component and drops control characters, so the
/// name can be shown safely (it must still never be used as a path).
pub fn clean_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    // Also drop characters that are special in Windows paths (`:` streams, wildcards).
    let cleaned: String = base.chars().filter(|c| !c.is_control() && !":<>\"|?*".contains(*c)).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4);
    if cleaned.is_empty() {
        "upload".into()
    } else if reserved {
        format!("_{}", cleaned.chars().take(199).collect::<String>())
    } else {
        cleaned.chars().take(200).collect()
    }
}

pub type Parsed = (Vec<(String, String)>, Vec<UploadedFile>);

pub fn parse(body: &[u8], boundary: &str) -> Result<Parsed, &'static str> {
    let delim = format!("--{boundary}").into_bytes();
    let sep = [b"\r\n".as_slice(), &delim].concat();
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut pos = find(body, &delim, 0).ok_or("missing multipart boundary")? + delim.len();
    for _ in 0..MAX_PARTS {
        if body[pos..].starts_with(b"--") {
            return Ok((fields, files));
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err("malformed multipart body");
        }
        pos += 2;
        let header_end = find(body, b"\r\n\r\n", pos).ok_or("malformed multipart headers")?;
        let headers = String::from_utf8_lossy(&body[pos..header_end]).into_owned();
        let data_start = header_end + 4;
        let data_end = find(body, &sep, data_start).ok_or("unterminated multipart part")?;
        let data = &body[data_start..data_end];
        pos = data_end + sep.len();

        let mut disposition = None;
        let mut content_type = "application/octet-stream".to_string();
        for line in headers.split("\r\n") {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-disposition") {
                    disposition = Some(v.trim().to_string());
                } else if k.trim().eq_ignore_ascii_case("content-type") {
                    content_type = v.trim().chars().filter(|c| !c.is_control()).take(200).collect();
                }
            }
        }
        let Some(disposition) = disposition else { continue };
        let Some(name) = param(&disposition, "name") else { continue };
        match param(&disposition, "filename") {
            Some(filename) => files.push(UploadedFile {
                field: name,
                filename: clean_filename(&filename),
                content_type,
                data: data.to_vec(),
            }),
            None => fields.push((name, String::from_utf8_lossy(data).into_owned())),
        }
    }
    Err("too many parts in multipart body")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_and_files() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHello\r\n--XyZ\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"../../etc/pa\x01sswd\"\r\nContent-Type: text/plain\r\n\r\nline1\r\nline2\r\n--XyZ--\r\n";
        let (fields, files) = parse(body, "XyZ").unwrap();
        assert_eq!(fields, vec![("title".to_string(), "Hello".to_string())]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "passwd");
        assert_eq!(files[0].data, b"line1\r\nline2");
        assert_eq!(boundary("multipart/form-data; boundary=\"XyZ\"").as_deref(), Some("XyZ"));
        assert!(parse(b"garbage", "XyZ").is_err());
        assert_eq!(clean_filename("C:\\Users\\a\\.htaccess"), "htaccess");
        assert_eq!(clean_filename("a.txt:stream"), "a.txtstream");
        assert_eq!(clean_filename("evil.php. . "), "evil.php");
        assert_eq!(clean_filename("CON.txt"), "_CON.txt");
    }
}
