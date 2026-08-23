pub(crate) fn parse_canonical_uuid(input: &str) -> Option<[u8; 16]> {
    if input.len() != 36
        || input.as_bytes().get(8) != Some(&b'-')
        || input.as_bytes().get(13) != Some(&b'-')
        || input.as_bytes().get(18) != Some(&b'-')
        || input.as_bytes().get(23) != Some(&b'-')
    {
        return None;
    }
    let compact = input
        .bytes()
        .filter(|byte| *byte != b'-')
        .collect::<Vec<_>>();
    if compact.len() != 32 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (index, pair) in compact.chunks_exact(2).enumerate() {
        bytes[index] = hex_digit(pair[0])?.checked_mul(16)? + hex_digit(pair[1])?;
    }
    Some(bytes)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
