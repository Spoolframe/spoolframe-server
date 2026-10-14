//! Moved from `bridge_tests.rs` unchanged: it reads no playback.
use super::*;

#[test]
fn malformed_and_unsatisfiable_ranges_never_underflow() {
    for range in ["bytes=9-2", "bytes=-0", "bytes=0-1,3-4", "bytes=+1-2", "bytes=18446744073709551616-"] {
        assert_eq!(parse_range(range), None, "{range}");
        assert!(range_bounds(10, Some(parse_range(range))).is_err());
    }
    assert!(range_bounds(0, Some(parse_range("bytes=0-"))).is_err());
    assert!(range_bounds(10, Some(parse_range("bytes=10-"))).is_err());
    assert_eq!(range_bounds(10, Some(parse_range("bytes=-3"))), Ok((7, 9, true)));
    assert_eq!(range_bounds(10, Some(parse_range("bytes=-30"))), Ok((0, 9, true)));
    assert_eq!(range_bounds(10, Some(parse_range("bytes=8-99"))), Ok((8, 9, true)));
    assert_eq!(range_bounds(0, None), Ok((0, 0, false)));
}
