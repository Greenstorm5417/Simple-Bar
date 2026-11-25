//! dwl support through its `dwl-ipc-unstable-v2` Wayland protocol (the IPC
//! patch), mirroring waybar's `dwl/tags` and `dwl/window` modules.
//!
//! dwl double-buffers its per-output events: tags, title and layout symbol
//! arrive one by one and a `frame` event closes the batch. [`OutputState`]
//! collects a batch and turns it into a [`Mon`] on `frame`.

use std::borrow::Cow;

use crate::mango::{Mon, Tag};

/// Generated bindings for `protocols/dwl-ipc-unstable-v2.xml`.
#[allow(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    unused_qualifications,
    clippy::all,
    clippy::pedantic
)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/dwl-ipc-unstable-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/dwl-ipc-unstable-v2.xml");
}

pub use protocol::zdwl_ipc_manager_v2::ZdwlIpcManagerV2;
pub use protocol::zdwl_ipc_output_v2::{self, ZdwlIpcOutputV2};

/// `tag_state.active`.
const TAG_ACTIVE: u32 = 1;
/// `tag_state.urgent`.
const TAG_URGENT: u32 = 2;

/// One output's state as dwl is reporting it, before the closing `frame`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputState {
    active: bool,
    tags: Vec<Tag>,
    title: String,
    layout: String,
}

impl OutputState {
    /// Fold one event into the batch. Returns true on `frame`, when the
    /// batch is complete and [`OutputState::to_mon`] should be applied.
    pub fn apply(&mut self, event: zdwl_ipc_output_v2::Event) -> bool {
        use zdwl_ipc_output_v2::Event;
        match event {
            Event::Active { active } => self.active = active != 0,
            Event::Tag { tag, state, .. } => {
                let state = u32::from(state);
                let Ok(index) = usize::try_from(tag) else {
                    return false;
                };
                if self.tags.len() <= index {
                    self.tags.resize_with(index + 1, || Tag {
                        index: 0,
                        active: false,
                        urgent: false,
                    });
                }
                self.tags[index] = Tag {
                    // Waybar labels dwl's 0-based tags from 1.
                    index: u8::try_from(index + 1).unwrap_or(u8::MAX),
                    active: state & TAG_ACTIVE != 0,
                    urgent: state & TAG_URGENT != 0,
                };
            }
            Event::Title { title } => self.title = title,
            Event::LayoutSymbol { layout } => self.layout = layout,
            Event::Frame => return true,
            _ => {}
        }
        false
    }

    /// The bar's view of this output.
    #[must_use]
    pub fn to_mon(&self, name: &str) -> Mon {
        Mon {
            name: name.to_owned(),
            active: self.active,
            title: self.title.chars().filter(|ch| !ch.is_control()).collect(),
            layout_text: Cow::Owned(self.layout.clone()),
            tags: self.tags.clone(),
        }
    }

    /// Bitmask of the tags currently in view.
    #[must_use]
    pub fn active_mask(&self) -> u32 {
        self.tags
            .iter()
            .enumerate()
            .filter(|(_, tag)| tag.active)
            .fold(0, |mask, (index, _)| mask | 1 << index)
    }
}

/// `set_tags` arguments for a left click on the 1-based tag `index`:
/// view only that tag, swapping dwl's tagset like waybar does.
#[must_use]
pub const fn view_tag(index: u8) -> (u32, u32) {
    (tag_bit(index), 1)
}

/// `set_tags` arguments for a right click: toggle the tag in the view.
#[must_use]
pub const fn toggle_tag(index: u8, active_mask: u32) -> (u32, u32) {
    (active_mask ^ tag_bit(index), 0)
}

const fn tag_bit(index: u8) -> u32 {
    match index.checked_sub(1) {
        Some(zero_based) if zero_based < 32 => 1 << zero_based,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zdwl_ipc_output_v2::{Event, TagState};

    fn tag(tag: u32, state: TagState) -> Event {
        Event::Tag {
            tag,
            state: wayland_client::WEnum::Value(state),
            clients: 1,
            focused: 0,
        }
    }

    #[test]
    fn a_frame_completes_the_batch() {
        let mut out = OutputState::default();
        assert!(!out.apply(Event::Active { active: 1 }));
        assert!(!out.apply(tag(0, TagState::None)));
        assert!(!out.apply(tag(1, TagState::Active)));
        assert!(!out.apply(tag(2, TagState::Urgent)));
        assert!(!out.apply(Event::Title {
            title: "foot\u{7}".into()
        }));
        assert!(!out.apply(Event::LayoutSymbol {
            layout: "[2]".into()
        }));
        assert!(out.apply(Event::Frame));
        let mon = out.to_mon("eDP-1");
        assert_eq!(mon.title, "foot");
        assert_eq!(mon.layout_text, "[2]");
        assert!(mon.active);
        assert_eq!(mon.tags.len(), 3);
        assert_eq!(
            mon.tags[1],
            Tag {
                index: 2,
                active: true,
                urgent: false
            }
        );
        assert!(mon.tags[2].urgent);
        assert_eq!(out.active_mask(), 0b10);
    }

    #[test]
    fn clicks_match_waybars_dwl_tags_module() {
        assert_eq!(view_tag(1), (0b1, 1));
        assert_eq!(view_tag(10), (1 << 9, 1));
        assert_eq!(toggle_tag(3, 0b001), (0b101, 0));
        assert_eq!(toggle_tag(1, 0b101), (0b100, 0));
    }
}
