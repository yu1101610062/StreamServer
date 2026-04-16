use super::*;

#[test]
fn parses_protocols() {
    let output = r#"
Supported file protocols:
Input:
  async
  http
Output:
  file
  rtmp
"#;

    assert_eq!(
        parse_ffmpeg_protocols(output),
        vec!["async", "file", "http", "rtmp"]
    );
}

#[test]
fn parses_formats() {
    let output = r#"
File formats:
 D  matroska,webm    Matroska / WebM
  E flv              FLV (Flash Video)
"#;

    assert_eq!(
        parse_ffmpeg_formats(output),
        vec!["flv", "matroska", "webm"]
    );
}

#[test]
fn parses_codecs() {
    let output = r#"
Encoders:
 V....D libx264             libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10
 A..... aac                AAC (Advanced Audio Coding)
"#;

    assert_eq!(parse_ffmpeg_codecs(output), vec!["aac", "libx264"]);
}

#[test]
fn extracts_zlm_rtmp_enhanced_flag_from_key_value_entries() {
    let value = serde_json::json!({
        "code": 0,
        "data": [
            {"key": "general.mediaServerId", "value": "zlm-1"},
            {"key": "rtmp.enhanced", "value": "1"}
        ]
    });

    assert_eq!(extract_zlm_rtmp_enhanced_enabled(&value), Some(true));
}

#[test]
fn extracts_zlm_rtmp_enhanced_flag_from_flat_object() {
    let value = serde_json::json!({
        "rtmp.enhanced": 0
    });

    assert_eq!(extract_zlm_rtmp_enhanced_enabled(&value), Some(false));
}
