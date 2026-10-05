//! PIN syntax shared by clients and servers; authentication remains server-side.

pub const MIN_DIGITS: usize = 4;
pub const MAX_DIGITS: usize = 8;

pub fn pin_digits(pin: &str) -> Option<u8> {
    ((MIN_DIGITS..=MAX_DIGITS).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit()))
        .then_some(pin.len() as u8)
}
