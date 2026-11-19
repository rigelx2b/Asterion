//! OpenDART corporation code list (`corpCode.xml`): every company DART knows, including
//! delisted ones, with its 8-digit corp code, name, current stock code and last change date.

use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::error::DataError;
use crate::opendart::DartClient;

pub const URL: &str = "https://opendart.fss.or.kr/api/corpCode.xml";
pub const DEFAULT_PATH: &str = "data/raw/opendart_corp/CORPCODE.xml";

/// One `<list>` entry. `stock_code` is empty for companies without a current listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpEntry {
    pub corp_code: String,
    pub corp_name: String,
    pub stock_code: String,
    pub modify_date: String,
}

/// Downloads the zipped list and returns the XML text inside it.
pub async fn fetch_xml(client: &mut DartClient, url: &str) -> Result<String, DataError> {
    let bytes = client.get_endpoint_bytes(url, &[]).await?;
    unzip_xml(&bytes)
}

// Errors come back as a small XML/JSON status document instead of a zip archive.
pub fn unzip_xml(bytes: &[u8]) -> Result<String, DataError> {
    if !bytes.starts_with(b"PK") {
        let text = String::from_utf8_lossy(bytes);
        return Err(DataError::Api {
            code: tag(&text, "status").unwrap_or_default(),
            message: tag(&text, "message").unwrap_or_else(|| text.chars().take(200).collect()),
        });
    }
    let invalid = |e: String| DataError::InvalidField {
        field: "corpCode.zip",
        value: e,
    };
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| invalid(e.to_string()))?;
    let mut file = archive.by_index(0).map_err(|e| invalid(e.to_string()))?;
    let mut xml = String::new();
    file.read_to_string(&mut xml)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(xml)
}

// Text of the first `<name>...</name>` element, trimmed; the list has no nesting or attributes.
fn tag(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim().to_owned())
}

pub fn parse(xml: &str) -> Vec<CorpEntry> {
    xml.split("<list>")
        .skip(1)
        .filter_map(|chunk| {
            let chunk = chunk.split("</list>").next()?;
            let corp_code = tag(chunk, "corp_code")?;
            (corp_code.len() == 8).then(|| CorpEntry {
                corp_code,
                corp_name: tag(chunk, "corp_name").unwrap_or_default(),
                stock_code: tag(chunk, "stock_code").unwrap_or_default(),
                modify_date: tag(chunk, "modify_date").unwrap_or_default(),
            })
        })
        .collect()
}

pub fn load(path: &Path) -> Result<Vec<CorpEntry>, DataError> {
    let xml = fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
    Ok(parse(&xml))
}

pub fn save(path: &Path, xml: &str) -> Result<(), DataError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| DataError::io(dir, e))?;
    }
    let tmp = path.with_extension("xml.tmp");
    fs::write(&tmp, xml).map_err(|e| DataError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| DataError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<result>\n<list>\n<corp_code>00434003</corp_code>\n<corp_name>다코</corp_name>\n<corp_eng_name>Daco corporation</corp_eng_name>\n<stock_code> </stock_code>\n<modify_date>20170630</modify_date>\n</list>\n<list>\n<corp_code>00126380</corp_code>\n<corp_name>삼성전자</corp_name>\n<stock_code>005930</stock_code>\n<modify_date>20250101</modify_date>\n</list>\n</result>";

    #[test]
    fn parses_entries_and_blank_stock_codes() {
        let entries = parse(XML);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].corp_code, "00434003");
        assert_eq!(entries[0].stock_code, "");
        assert_eq!(entries[1].stock_code, "005930");
        assert_eq!(entries[1].corp_name, "삼성전자");
    }

    #[test]
    fn non_zip_body_is_an_api_error() {
        let body = b"<?xml version=\"1.0\"?><result><status>010</status><message>bad key</message></result>";
        match unzip_xml(body) {
            Err(DataError::Api { code, message }) => {
                assert_eq!(code, "010");
                assert_eq!(message, "bad key");
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }
}
