//! Where guest memory comes from and where the guest sees it, as a value.
//!
//! An [`Object`] is a logical store of bytes; a [`Window`] places one slice of
//! an object in the guest-physical address space. A layout holds guest
//! placement only: how the host materializes objects is a separate policy, and
//! host addresses, fds and memslots never enter it.

use std::collections::BTreeMap;
use std::fmt;

use crate::align::GuestPageSize;
use crate::arch::x86_64::layout::{LayoutError, ram_regions};
use crate::space::{Gpa, ObjectOffset, Range};

/// Names an object within a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u32);

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Names a window within a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

impl fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Logical bytes for windows to place in the guest. It is an identity, not a
/// mapping or a file: the host may map an object once per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    pub capacity: u64,
}

/// Who a window belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowOwner {
    /// The machine's boot memory, bound before the guest runs.
    Boot,
}

/// What a window holds for the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// RAM the guest finds in its boot memory map.
    BootRam,
}

/// A slice of an object, placed in the guest. The slice's length is the
/// window's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    object: ObjectId,
    slice: Range<ObjectOffset>,
    placement: Range<Gpa>,
    owner: WindowOwner,
    kind: WindowKind,
}

impl Window {
    /// Places `slice` of `object` in the guest from `start`; `None` if it would
    /// run past the top of the guest-physical address space.
    pub fn new(
        object: ObjectId,
        slice: Range<ObjectOffset>,
        start: Gpa,
        owner: WindowOwner,
        kind: WindowKind,
    ) -> Option<Self> {
        Some(Self {
            object,
            slice,
            placement: Range::new(start, slice.len())?,
            owner,
            kind,
        })
    }

    pub fn object(&self) -> ObjectId {
        self.object
    }

    pub fn slice(&self) -> Range<ObjectOffset> {
        self.slice
    }

    pub fn placement(&self) -> Range<Gpa> {
        self.placement
    }

    pub fn owner(&self) -> WindowOwner {
        self.owner
    }

    pub fn kind(&self) -> WindowKind {
        self.kind
    }
}

impl fmt::Display for Window {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} of object {} at {}",
            self.slice, self.object, self.placement
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum MemoryLayoutError {
    /// lay out boot RAM
    BootRam(#[source] LayoutError),
    /// object {0} is listed twice
    DuplicateObject(ObjectId),
    /// window {0} is listed twice
    DuplicateWindow(WindowId),
    /// window {id} places object {object}, which the layout lacks
    UnknownObject { id: WindowId, object: ObjectId },
    /// window {0} is empty
    EmptyWindow(WindowId),
    /// window {id} ({window}) is not aligned to the guest page
    Unaligned { id: WindowId, window: Window },
    /// window {id} ({window}) runs past the {capacity:#x} bytes of its object
    OutsideObject {
        id: WindowId,
        window: Window,
        capacity: u64,
    },
    /// windows {id} and {other} overlap in guest-physical memory
    Overlap { id: WindowId, other: WindowId },
    /// windows {id} and {other} both place the same bytes of object {object}
    Alias {
        id: WindowId,
        other: WindowId,
        object: ObjectId,
    },
    /// memory layout has no boot RAM
    NoBootRam,
}

/// Guest memory as objects and the windows that place them.
///
/// Only [`MemoryLayout::new`] makes one, so every layout has boot RAM, and its
/// windows are non-empty, aligned to the guest page, inside their objects,
/// disjoint in guest-physical memory, and never place the same object bytes
/// twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLayout {
    objects: BTreeMap<ObjectId, Object>,
    windows: BTreeMap<WindowId, Window>,
}

impl MemoryLayout {
    pub fn new(
        objects: impl IntoIterator<Item = (ObjectId, Object)>,
        windows: impl IntoIterator<Item = (WindowId, Window)>,
    ) -> Result<Self, MemoryLayoutError> {
        let mut layout = Self {
            objects: BTreeMap::new(),
            windows: BTreeMap::new(),
        };
        for (id, object) in objects {
            if layout.objects.insert(id, object).is_some() {
                return Err(MemoryLayoutError::DuplicateObject(id));
            }
        }
        for (id, window) in windows {
            if layout.windows.contains_key(&id) {
                return Err(MemoryLayoutError::DuplicateWindow(id));
            }
            layout.check(id, window)?;
            layout.windows.insert(id, window);
        }
        if !layout
            .windows
            .values()
            .any(|window| window.kind == WindowKind::BootRam)
        {
            return Err(MemoryLayoutError::NoBootRam);
        }
        Ok(layout)
    }

    /// Boot RAM of `size` bytes: one object, placed around the 32-bit MMIO
    /// hole.
    pub fn boot(size: u64) -> Result<Self, MemoryLayoutError> {
        const OBJECT: ObjectId = ObjectId(0);
        let overflow = MemoryLayoutError::BootRam(LayoutError::Overflow);
        let regions = ram_regions(size).map_err(MemoryLayoutError::BootRam)?;
        let mut windows = Vec::with_capacity(regions.len());
        let mut next = ObjectOffset(0);
        for (id, placement) in (0..).map(WindowId).zip(regions) {
            let slice = Range::new(next, placement.len()).ok_or(overflow)?;
            next = slice.end().ok_or(overflow)?;
            let window = Window::new(
                OBJECT,
                slice,
                placement.start(),
                WindowOwner::Boot,
                WindowKind::BootRam,
            )
            .ok_or(overflow)?;
            windows.push((id, window));
        }
        Self::new([(OBJECT, Object { capacity: size })], windows)
    }

    pub fn objects(&self) -> impl Iterator<Item = (ObjectId, &Object)> {
        self.objects.iter().map(|(&id, object)| (id, object))
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        self.windows.iter().map(|(&id, window)| (id, window))
    }

    /// The windows of boot memory, which are bound before the guest runs.
    pub fn boot_windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        self.windows()
            .filter(|(_, window)| window.owner == WindowOwner::Boot)
    }

    /// Checks `window` against the objects and the windows already admitted.
    fn check(&self, id: WindowId, window: Window) -> Result<(), MemoryLayoutError> {
        let Some(object) = self.objects.get(&window.object) else {
            return Err(MemoryLayoutError::UnknownObject {
                id,
                object: window.object,
            });
        };
        if window.slice.is_empty() {
            return Err(MemoryLayoutError::EmptyWindow(id));
        }
        let page = GuestPageSize::X86_4K.alignment();
        if !window.slice.is_aligned(page) || !window.placement.is_aligned(page) {
            return Err(MemoryLayoutError::Unaligned { id, window });
        }
        if !window
            .slice
            .end()
            .is_some_and(|end| end.0 <= object.capacity)
        {
            return Err(MemoryLayoutError::OutsideObject {
                id,
                window,
                capacity: object.capacity,
            });
        }
        for (&other, admitted) in &self.windows {
            if admitted.placement.overlaps(window.placement) {
                return Err(MemoryLayoutError::Overlap { id, other });
            }
            if admitted.object == window.object && admitted.slice.overlaps(window.slice) {
                return Err(MemoryLayoutError::Alias {
                    id,
                    other,
                    object: window.object,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ops;

    use super::*;

    const OBJECT: ObjectId = ObjectId(0);

    fn slice(range: ops::Range<u64>) -> Range<ObjectOffset> {
        range.try_into().unwrap()
    }

    fn gpa(range: ops::Range<u64>) -> Range<Gpa> {
        range.try_into().unwrap()
    }

    fn window(object: ObjectId, offsets: ops::Range<u64>, start: u64) -> Window {
        Window::new(
            object,
            slice(offsets),
            Gpa(start),
            WindowOwner::Boot,
            WindowKind::BootRam,
        )
        .unwrap()
    }

    /// Objects `(id, capacity)`, and windows numbered in order.
    fn layout(
        objects: &[(u32, u64)],
        windows: &[Window],
    ) -> Result<MemoryLayout, MemoryLayoutError> {
        MemoryLayout::new(
            objects
                .iter()
                .map(|&(id, capacity)| (ObjectId(id), Object { capacity })),
            (0..).map(WindowId).zip(windows.iter().copied()),
        )
    }

    fn placed(layout: &MemoryLayout) -> Vec<(WindowId, Range<ObjectOffset>, Range<Gpa>)> {
        layout
            .windows()
            .map(|(id, window)| (id, window.slice(), window.placement()))
            .collect()
    }

    #[test]
    fn boot_memory_is_one_object_placed_around_the_mmio_hole() {
        let low = MemoryLayout::boot(128 << 20).unwrap();
        assert_eq!(
            low.objects().collect::<Vec<_>>(),
            [(
                OBJECT,
                &Object {
                    capacity: 128 << 20
                }
            )]
        );
        assert_eq!(
            placed(&low),
            [(WindowId(0), slice(0..128 << 20), gpa(0..128 << 20))]
        );

        let split = MemoryLayout::boot(4 << 30).unwrap();
        assert_eq!(
            split.objects().collect::<Vec<_>>(),
            [(OBJECT, &Object { capacity: 4 << 30 })]
        );
        assert_eq!(
            placed(&split),
            [
                (WindowId(0), slice(0..3 << 30), gpa(0..3 << 30)),
                (WindowId(1), slice(3 << 30..4 << 30), gpa(4 << 30..5 << 30)),
            ]
        );
        assert_eq!(split.boot_windows().count(), 2);
        for (_, window) in split.windows() {
            assert_eq!(window.object(), OBJECT);
            assert_eq!(window.owner(), WindowOwner::Boot);
            assert_eq!(window.kind(), WindowKind::BootRam);
        }
    }

    #[test]
    fn boot_sizes_follow_the_ram_layout_rules() {
        for (size, error) in [
            (0, LayoutError::Empty),
            (0x1800, LayoutError::Unaligned(0x1800)),
            (u64::MAX - 0xfff, LayoutError::Overflow),
        ] {
            assert_eq!(
                MemoryLayout::boot(size),
                Err(MemoryLayoutError::BootRam(error))
            );
        }
        // The largest boot memory ends exactly at the top of the address space.
        let largest = MemoryLayout::boot(u64::MAX - (1 << 30) + 1).unwrap();
        let high = largest.window(WindowId(1)).unwrap();
        assert_eq!(high.placement().last(), Some(Gpa(u64::MAX)));
        assert_eq!(
            high.slice().end(),
            Some(ObjectOffset(u64::MAX - (1 << 30) + 1))
        );
    }

    #[test]
    fn windows_may_share_an_object_without_aliasing_it() {
        let layout = layout(
            &[(0, 0x3000), (1, 0x2000)],
            &[
                window(OBJECT, 0..0x2000, 0),
                window(OBJECT, 0x2000..0x3000, 1 << 32),
                // Another object's bytes, at offsets the first object also uses.
                window(ObjectId(1), 0..0x2000, 0x8000),
            ],
        )
        .unwrap();
        assert_eq!(
            placed(&layout),
            [
                (WindowId(0), slice(0..0x2000), gpa(0..0x2000)),
                (
                    WindowId(1),
                    slice(0x2000..0x3000),
                    gpa(1 << 32..(1 << 32) + 0x1000)
                ),
                (WindowId(2), slice(0..0x2000), gpa(0x8000..0xa000)),
            ]
        );
        assert_eq!(layout.window(WindowId(3)), None);
        // Layouts are values: a copy is equal, and a different window is not.
        let copy = layout.clone();
        assert_eq!(copy, layout);
        assert_ne!(copy, MemoryLayout::boot(0x3000).unwrap());
    }

    #[test]
    fn invalid_layouts_are_rejected() {
        let objects = [(0, 0x4000), (1, 0x4000)];
        let low = window(OBJECT, 0..0x2000, 0);
        assert_eq!(
            layout(&objects, &[window(ObjectId(2), 0..0x1000, 0)]),
            Err(MemoryLayoutError::UnknownObject {
                id: WindowId(0),
                object: ObjectId(2),
            })
        );
        assert_eq!(
            layout(&objects, &[window(OBJECT, 0x1000..0x1000, 0x1000)]),
            Err(MemoryLayoutError::EmptyWindow(WindowId(0)))
        );
        for unaligned in [
            window(OBJECT, 0x800..0x1800, 0x1000),
            window(OBJECT, 0x1000..0x2800, 0x1000),
            window(OBJECT, 0x1000..0x2000, 0x1800),
        ] {
            assert_eq!(
                layout(&objects, &[unaligned]),
                Err(MemoryLayoutError::Unaligned {
                    id: WindowId(0),
                    window: unaligned,
                })
            );
        }
        let past = window(OBJECT, 0x3000..0x5000, 0);
        assert_eq!(
            layout(&objects, &[past]),
            Err(MemoryLayoutError::OutsideObject {
                id: WindowId(0),
                window: past,
                capacity: 0x4000,
            })
        );
        // No object holds the last byte of the offset space.
        let top = Window::new(
            OBJECT,
            Range::new(ObjectOffset(u64::MAX - 0xfff), 0x1000).unwrap(),
            Gpa(0),
            WindowOwner::Boot,
            WindowKind::BootRam,
        )
        .unwrap();
        assert_eq!(
            layout(&[(0, u64::MAX)], &[top]),
            Err(MemoryLayoutError::OutsideObject {
                id: WindowId(0),
                window: top,
                capacity: u64::MAX,
            })
        );
        assert_eq!(
            layout(&objects, &[low, window(ObjectId(1), 0..0x1000, 0x1000)]),
            Err(MemoryLayoutError::Overlap {
                id: WindowId(1),
                other: WindowId(0),
            })
        );
        assert_eq!(
            layout(&objects, &[low, window(OBJECT, 0x1000..0x2000, 1 << 32)]),
            Err(MemoryLayoutError::Alias {
                id: WindowId(1),
                other: WindowId(0),
                object: OBJECT,
            })
        );
        assert_eq!(layout(&objects, &[]), Err(MemoryLayoutError::NoBootRam));
        assert_eq!(
            Window::new(
                OBJECT,
                slice(0..0x2000),
                Gpa(u64::MAX - 0xfff),
                WindowOwner::Boot,
                WindowKind::BootRam,
            ),
            None
        );
    }

    #[test]
    fn ids_are_unique() {
        let object = Object { capacity: 0x4000 };
        assert_eq!(
            MemoryLayout::new([(OBJECT, object), (OBJECT, object)], []),
            Err(MemoryLayoutError::DuplicateObject(OBJECT))
        );
        assert_eq!(
            MemoryLayout::new(
                [(OBJECT, object)],
                [
                    (WindowId(0), window(OBJECT, 0..0x1000, 0)),
                    (WindowId(0), window(OBJECT, 0x1000..0x2000, 0x1000)),
                ],
            ),
            Err(MemoryLayoutError::DuplicateWindow(WindowId(0)))
        );
    }

    #[test]
    fn errors_name_the_windows_involved() {
        let low = window(OBJECT, 0..0x2000, 0);
        assert_eq!(
            layout(&[(0, 0x1000)], &[low]).unwrap_err().to_string(),
            "window 0 (0x0..=0x1fff of object 0 at 0x0..=0x1fff) runs past the 0x1000 bytes \
             of its object"
        );
        assert_eq!(
            layout(
                &[(0, 0x4000)],
                &[low, window(OBJECT, 0x1000..0x2000, 1 << 32)]
            )
            .unwrap_err()
            .to_string(),
            "windows 1 and 0 both place the same bytes of object 0"
        );
        let error = MemoryLayout::boot(0).unwrap_err();
        assert_eq!(error.to_string(), "lay out boot RAM");
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "guest memory size must be non-zero"
        );
    }
}
