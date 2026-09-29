// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! This module defines the [`LayoutTree`][layout_tree::LayoutTree] data
//! structure, on which all layout logic is defined.

pub mod contexts;
mod layout_mapping;
mod layout_tree;
pub mod native_tabs;
mod parking;
mod scroll_constraints;
pub mod scroll_viewport;
mod selection;
mod size;
pub mod spring;
mod tree;
mod window;

pub use layout_mapping::SpaceLayoutMapping;
pub use layout_tree::{LayoutId, LayoutKind, LayoutTree};
pub use parking::{BottomCorner, accepted_bottom_strip, bounded_bottom_corner};
pub use size::{ContainerKind, Direction, GroupBarInfo, Orientation};
pub use tree::NodeId;
