//! Shared `Rgba` → packed `u32` shim used by every Shell render module.
//!
//! gpui's `rgb()` constructor takes a packed `u32` while our theme
//! tokens are `Rgba` floats — every render module ends up needing this
//! exact shim.  Pulled into one place so a future gpui API change is a
//! one-file fix.

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts.  Alpha
/// is dropped — gpui composes opacity through its own builders, not
/// the packed channel.
pub fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_theme::colors::{BRAND, SURFACE_900};

    #[test]
    fn pack_surface_900_matches_hex() {
        assert_eq!(pack(SURFACE_900), 0x0a0e17);
    }

    #[test]
    fn pack_brand_matches_hex() {
        assert_eq!(pack(BRAND), 0x0e7490);
    }

    #[test]
    fn pack_clamps_oversaturated() {
        let weird = gpui::Rgba {
            r: 2.0,
            g: -1.0,
            b: 0.5,
            a: 1.0,
        };
        let packed = pack(weird);
        assert_eq!(packed >> 16, 0xff);
        assert_eq!((packed >> 8) & 0xff, 0x00);
        assert!((0x7e..=0x80).contains(&(packed & 0xff)));
    }
}
