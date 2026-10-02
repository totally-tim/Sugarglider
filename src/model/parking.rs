// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where to put a window to hide it while keeping it on its screen.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::sys::geometry::{CGRectExt, SameAs};

/// The horizontal edge that leaves a narrow strip at a display's bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BottomCorner {
    Left,
    Right,
}

/// A bottom corner that has no requested overlap with another display.
pub fn bounded_bottom_corner(
    size: CGSize,
    bounds: CGRect,
    others: &[CGRect],
) -> Option<(CGPoint, BottomCorner)> {
    if !valid_rect(bounds) || !valid_size(size) {
        return None;
    }
    let bottom = bounds.max().y - 1.0;
    let corners = [
        (CGPoint::new(bounds.max().x - 1.0, bottom), BottomCorner::Right),
        (
            CGPoint::new(bounds.min().x - size.width + 1.0, bottom),
            BottomCorner::Left,
        ),
    ];
    corners.into_iter().find(|(origin, _)| {
        let frame = CGRect { origin: *origin, size };
        others.iter().all(|other| other.intersection(&frame).area() == 0.0)
    })
}

/// The tallest strip a parked window may leave on its display.
pub const MAX_STRIP_HEIGHT: f64 = 64.0;

/// Accepts an AX readback only when it leaves a strip no wider than one
/// point and no taller than [`MAX_STRIP_HEIGHT`] points at the selected
/// bottom corner.
pub fn accepted_bottom_strip(
    observed: CGRect,
    requested_size: CGSize,
    bounds: CGRect,
    others: &[CGRect],
    corner: BottomCorner,
) -> bool {
    if !valid_rect(observed) || !valid_rect(bounds) || !observed.size.same_as(requested_size) {
        return false;
    }
    let strip = observed.intersection(&bounds);
    let touches_side = match corner {
        BottomCorner::Left => strip.min().x == bounds.min().x,
        BottomCorner::Right => strip.max().x == bounds.max().x,
    };
    strip.size.width > 0.0
        && strip.size.width <= 1.0
        && strip.size.height > 0.0
        && strip.size.height <= MAX_STRIP_HEIGHT
        && strip.max().y == bounds.max().y
        && touches_side
        && others.iter().all(|other| other.intersection(&observed).area() == 0.0)
}

fn valid_size(size: CGSize) -> bool {
    size.width.is_finite() && size.width > 0.0 && size.height.is_finite() && size.height > 0.0
}

fn valid_rect(rect: CGRect) -> bool {
    rect.origin.x.is_finite() && rect.origin.y.is_finite() && valid_size(rect.size)
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{BottomCorner, accepted_bottom_strip, bounded_bottom_corner};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    #[test]
    fn bottom_strip_accepts_textedit_clamp_but_never_widens_the_bound() {
        let bounds = rect(0., 0., 1512., 982.);
        let size = CGSize::new(586., 488.);
        assert_eq!(
            bounded_bottom_corner(size, bounds, &[]),
            Some((CGPoint::new(1511., 981.), BottomCorner::Right))
        );
        let accepted = rect(1511., 950., 586., 488.);
        assert!(accepted_bottom_strip(
            accepted,
            size,
            bounds,
            &[],
            BottomCorner::Right
        ));
        for accepted in [
            rect(1511., 951., 586., 488.),
            rect(1511., 941., 586., 488.),
            rect(1511., 918., 586., 488.),
        ] {
            assert!(accepted_bottom_strip(
                accepted,
                size,
                bounds,
                &[],
                BottomCorner::Right
            ));
        }
        for rejected in [
            rect(1510., 950., 586., 488.),
            rect(1511., 917., 586., 488.),
            rect(1511., 33., 586., 488.),
            rect(1511., 950., 587., 488.),
        ] {
            assert!(!accepted_bottom_strip(
                rejected,
                size,
                bounds,
                &[],
                BottomCorner::Right
            ));
        }
    }

    #[test]
    fn bottom_strip_uses_full_bounds_and_rejects_other_displays() {
        let own = rect(0., 0., 1000., 1000.);
        let right = rect(1000., 0., 1000., 1000.);
        let size = CGSize::new(400., 300.);
        assert_eq!(
            bounded_bottom_corner(size, own, &[right]),
            Some((CGPoint::new(-399., 999.), BottomCorner::Left))
        );
        assert!(!accepted_bottom_strip(
            rect(999., 968., 400., 300.),
            size,
            own,
            &[right],
            BottomCorner::Right
        ));
        assert!(accepted_bottom_strip(
            rect(-399., 968., 400., 300.),
            size,
            own,
            &[right],
            BottomCorner::Left
        ));
        let left = rect(-1000., 0., 1000., 1000.);
        assert_eq!(bounded_bottom_corner(size, own, &[left, right]), None);
    }

    #[test]
    fn an_upper_display_with_no_clear_bottom_corner_has_no_target() {
        let upper = rect(0., -1000., 1000., 1000.);
        let lower = rect(0., 0., 1000., 1000.);
        let size = CGSize::new(400., 300.);
        assert_eq!(bounded_bottom_corner(size, upper, &[lower]), None);
        assert_eq!(
            bounded_bottom_corner(size, lower, &[upper]),
            Some((CGPoint::new(999., 999.), BottomCorner::Right))
        );
    }
}
