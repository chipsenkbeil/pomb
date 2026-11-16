/// Returns `text` cut to at most `max` bytes at a character boundary.
#[must_use]
pub(crate) fn cut_to_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::cut_to_bytes;

    #[test]
    fn cut_to_bytes_should_keep_whole_characters_under_the_limit() {
        assert_eq!(cut_to_bytes("héllo", 2), "h");
        assert_eq!(cut_to_bytes("héllo", 3), "hé");
        assert_eq!(cut_to_bytes("héllo", 9), "héllo");
    }
}
