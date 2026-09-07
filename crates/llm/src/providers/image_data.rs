/// Split a base64 image data URI into the media type and encoded payload.
/// Remote URLs intentionally return `None` and keep each provider's existing path.
pub(crate) fn split_base64_image_uri(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if !media_type.starts_with("image/") || media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_image_base64_data_uri() {
        assert_eq!(
            split_base64_image_uri("data:image/png;base64,AAAA"),
            Some(("image/png", "AAAA"))
        );
        assert_eq!(split_base64_image_uri("https://example.com/a.png"), None);
        assert_eq!(split_base64_image_uri("data:text/plain;base64,AAAA"), None);
    }
}
