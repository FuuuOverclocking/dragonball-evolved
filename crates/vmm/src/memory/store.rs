//! The host bytes of a memory layout.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::sync::Arc;

use rustix::fs::{MemfdFlags, SealFlags};
use rustix::io::Errno;
use vm_memory::mmap::{MmapRegionBuilder, MmapRegionError};
use vm_memory::{
    FileOffset, GuestAddress, GuestMemoryMmap, GuestMemoryRegion, GuestRegionCollectionError,
    GuestRegionMmap,
};

use crate::kvm_context::HostCapabilities;
use crate::memory::layout::{MemoryLayout, ObjectId, Window, WindowId};
use crate::memory::policy::{BackingKind, HostPolicy, HostPolicyError};

/// A view of the bytes of guest memory. Cloning it is cheap, and it keeps the
/// bytes mapped, but it cannot bind them to a VM.
pub type MemoryView = Arc<GuestMemoryMmap<()>>;

/// The host mapping of one window.
pub type WindowMapping = Arc<GuestRegionMmap<()>>;

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum MaterializeError {
    /// memory layout does not fit the host
    Policy(#[from] HostPolicyError),
    /// create the memfd of object {object}
    CreateMemfd {
        object: ObjectId,
        #[source]
        source: io::Error,
    },
    /// size the memfd of object {object} to {capacity:#x} bytes
    SizeMemfd {
        object: ObjectId,
        capacity: u64,
        #[source]
        source: io::Error,
    },
    /// seal the memfd of object {object}
    SealMemfd {
        object: ObjectId,
        #[source]
        source: io::Error,
    },
    /// window {0} does not fit the address space of this process
    Unmappable(WindowId),
    /// window {0} ends at the top of the guest-physical address space, which a
    /// memory view cannot hold
    EndsAtTop(WindowId),
    /// map window {window}
    Map {
        window: WindowId,
        #[source]
        source: MmapRegionError,
    },
    /// assemble the view of guest memory
    View(#[source] GuestRegionCollectionError),
}

/// Guest memory as the host holds it: a layout, materialized under a host
/// policy with one mapping per window.
///
/// It has a single owner, the only one that can hand it to a VM to bind. Views
/// of its bytes are cheap to clone but cannot be bound, and the store itself
/// cannot be cloned, so it never backs two VMs:
///
/// ```compile_fail,E0277
/// fn clone<T: Clone>() {}
/// clone::<vmm::memory::store::MemoryStore>();
/// ```
#[derive(Debug)]
pub struct MemoryStore {
    layout: MemoryLayout,
    policy: HostPolicy,
    view: MemoryView,
    mappings: BTreeMap<WindowId, WindowMapping>,
}

impl MemoryStore {
    /// Maps every window of `layout` as `policy` says, once the policy is
    /// checked against the layout and `host`.
    pub fn materialize(
        layout: MemoryLayout,
        policy: HostPolicy,
        host: &HostCapabilities,
    ) -> Result<Self, MaterializeError> {
        policy.check(&layout, host)?;
        let mut mappings = BTreeMap::new();
        for (object_id, object) in layout.objects() {
            let memfd = match policy.kind {
                BackingKind::AnonymousPrivate => None,
                BackingKind::SharedMemfd => Some(Arc::new(memfd(object_id, object.capacity)?)),
            };
            for (id, window) in layout.windows() {
                if window.object() == object_id {
                    mappings.insert(id, Arc::new(map(id, window, memfd.as_ref())?));
                }
            }
        }
        let mut regions: Vec<_> = mappings.values().cloned().collect();
        regions.sort_unstable_by_key(|region| region.start_addr());
        let view = GuestMemoryMmap::from_arc_regions(regions).map_err(MaterializeError::View)?;
        Ok(Self {
            layout,
            policy,
            view: Arc::new(view),
            mappings,
        })
    }

    /// The layout this memory was materialized from.
    pub fn layout(&self) -> &MemoryLayout {
        &self.layout
    }

    /// The policy this memory was materialized under.
    pub fn policy(&self) -> &HostPolicy {
        &self.policy
    }

    pub fn view(&self) -> &MemoryView {
        &self.view
    }

    /// The host mapping of `window`, which a KVM binding keeps alive.
    pub fn mapping(&self, window: WindowId) -> Option<&WindowMapping> {
        self.mappings.get(&window)
    }
}

/// Maps `window` from `memfd` at the window's object offset, or as private
/// anonymous memory without one.
fn map(
    id: WindowId,
    window: &Window,
    memfd: Option<&Arc<File>>,
) -> Result<GuestRegionMmap<()>, MaterializeError> {
    let len =
        usize::try_from(window.slice().len()).map_err(|_| MaterializeError::Unmappable(id))?;
    let builder =
        MmapRegionBuilder::<()>::new(len).with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE);
    let builder = match memfd {
        None => {
            builder.with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE)
        }
        Some(file) => builder
            .with_mmap_flags(libc::MAP_SHARED | libc::MAP_NORESERVE)
            .with_file_offset(FileOffset::from_arc(
                Arc::clone(file),
                window.slice().start().0,
            )),
    };
    let region = builder
        .build()
        .map_err(|source| MaterializeError::Map { window: id, source })?;
    GuestRegionMmap::new(region, GuestAddress(window.placement().start().0))
        .ok_or(MaterializeError::EndsAtTop(id))
}

/// A memfd of exactly `capacity` bytes, sealed at that size so a process it is
/// handed to never sees it shrink or grow.
fn memfd(object: ObjectId, capacity: u64) -> Result<File, MaterializeError> {
    let create = |flags| {
        rustix::fs::memfd_create(
            c"guest-memory",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING | flags,
        )
    };
    // Kernels from 6.3 may refuse a memfd that leaves exec unsaid; older ones reject the flag.
    let fd = match create(MemfdFlags::NOEXEC_SEAL) {
        Err(Errno::INVAL) => create(MemfdFlags::empty()),
        created => created,
    }
    .map_err(|errno| MaterializeError::CreateMemfd {
        object,
        source: errno.into(),
    })?;
    let file = File::from(fd);
    file.set_len(capacity)
        .map_err(|source| MaterializeError::SizeMemfd {
            object,
            capacity,
            source,
        })?;
    rustix::fs::fcntl_add_seals(&file, SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL)
        .map_err(|errno| MaterializeError::SealMemfd {
            object,
            source: errno.into(),
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::ops;
    use std::os::unix::fs::FileExt;

    use vm_memory::{Address, Bytes, GuestMemoryBackend};

    use super::*;
    use crate::align::BackingPageSize;
    use crate::memory::layout::{Object, WindowKind, WindowOwner};
    use crate::space::{Gpa, ObjectOffset, Range};

    const KINDS: [BackingKind; 2] = [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd];
    const HIGH: u64 = 1 << 32;

    fn host() -> HostCapabilities {
        HostCapabilities::fake(46, 32, 4096)
    }

    fn policy(kind: BackingKind) -> HostPolicy {
        HostPolicy {
            kind,
            page_size: BackingPageSize::base(host().page_size),
        }
    }

    fn window(object: u32, offsets: ops::Range<u64>, start: u64) -> Window {
        Window::new(
            ObjectId(object),
            offsets.try_into().unwrap(),
            Gpa(start),
            WindowOwner::Boot,
            WindowKind::BootRam,
        )
        .unwrap()
    }

    /// One object split across the 32-bit hole: its first two pages low, its
    /// third at 4 GiB; optionally a window over a second object.
    fn split(extra: bool) -> MemoryLayout {
        let mut objects = vec![(ObjectId(0), Object { capacity: 0x3000 })];
        let mut windows = vec![
            (WindowId(0), window(0, 0..0x2000, 0)),
            (WindowId(1), window(0, 0x2000..0x3000, HIGH)),
        ];
        if extra {
            objects.push((ObjectId(1), Object { capacity: 0x2000 }));
            windows.push((WindowId(2), window(1, 0..0x2000, 0x8000)));
        }
        MemoryLayout::new(objects, windows).unwrap()
    }

    fn store(layout: MemoryLayout, kind: BackingKind) -> MemoryStore {
        MemoryStore::materialize(layout, policy(kind), &host()).unwrap()
    }

    fn file(store: &MemoryStore, window: u32) -> &Arc<File> {
        store
            .mapping(WindowId(window))
            .unwrap()
            .file_offset()
            .unwrap()
            .arc()
    }

    #[test]
    fn split_windows_are_readable_and_writable_on_both_backings() {
        for kind in KINDS {
            let store = store(split(false), kind);
            assert_eq!(store.layout(), &split(false));
            assert_eq!(store.policy(), &policy(kind));
            let view = store.view();
            let mapped: Vec<_> = view
                .iter()
                .map(|region| {
                    let start = Gpa(region.start_addr().raw_value());
                    (
                        Range::new(start, region.len()).unwrap(),
                        region.file_offset().is_some(),
                    )
                })
                .collect();
            let shared = kind == BackingKind::SharedMemfd;
            assert_eq!(
                mapped,
                store
                    .layout()
                    .windows()
                    .map(|(_, window)| (window.placement(), shared))
                    .collect::<Vec<_>>()
            );
            view.write_obj(0x1234_u32, GuestAddress(0x1000)).unwrap();
            view.write_obj(0x5678_u32, GuestAddress(HIGH)).unwrap();
            assert_eq!(view.read_obj::<u32>(GuestAddress(0x1000)).unwrap(), 0x1234);
            assert_eq!(view.read_obj::<u32>(GuestAddress(HIGH)).unwrap(), 0x5678);
        }
    }

    #[test]
    fn memfd_objects_are_reachable_through_a_sealed_fd() {
        let store = store(split(false), BackingKind::SharedMemfd);
        store
            .view()
            .write_slice(b"guest", GuestAddress(HIGH + 0x10))
            .unwrap();
        let file = file(&store, 1);
        // Another process mapping the fd would see the guest's writes.
        let mut bytes = [0; 5];
        file.read_exact_at(&mut bytes, 0x2010).unwrap();
        assert_eq!(&bytes, b"guest");
        assert_eq!(file.metadata().unwrap().len(), 0x3000);
        assert!(file.set_len(0x4000).is_err(), "the memfd can still grow");
        assert!(file.set_len(0).is_err(), "the memfd can still shrink");
    }

    #[test]
    fn one_memfd_object_backs_each_window_at_its_object_offset() {
        let store = store(split(true), BackingKind::SharedMemfd);
        let view = store.view();
        let object = file(&store, 0);
        assert!(Arc::ptr_eq(object, file(&store, 1)));
        assert!(!Arc::ptr_eq(object, file(&store, 2)));
        for (id, window) in store.layout().windows() {
            let offset = store.mapping(id).unwrap().file_offset().unwrap().start();
            assert_eq!(ObjectOffset(offset), window.slice().start());
        }

        view.write_slice(b"low", GuestAddress(0x1ff0)).unwrap();
        view.write_slice(b"high", GuestAddress(HIGH + 0x20))
            .unwrap();
        let mut low = [0; 3];
        object.read_exact_at(&mut low, 0x1ff0).unwrap();
        assert_eq!(&low, b"low");
        let mut high = [0; 4];
        object.read_exact_at(&mut high, 0x2020).unwrap();
        assert_eq!(&high, b"high");
        // And the other way round: bytes written to the fd appear in both windows.
        object.write_all_at(b"fd", 0x0800).unwrap();
        object.write_all_at(b"fd", 0x2800).unwrap();
        let mut seen = [0; 2];
        view.read_slice(&mut seen, GuestAddress(0x0800)).unwrap();
        assert_eq!(&seen, b"fd");
        view.read_slice(&mut seen, GuestAddress(HIGH + 0x800))
            .unwrap();
        assert_eq!(&seen, b"fd");
    }

    #[test]
    fn extra_windows_are_mapped_beside_the_boot_windows() {
        for kind in KINDS {
            let store = store(split(true), kind);
            assert_eq!(store.layout(), &split(true));
            assert_eq!(store.view().num_regions(), 3);
            let extra = store.mapping(WindowId(2)).unwrap();
            assert_eq!(extra.start_addr(), GuestAddress(0x8000));
            assert_eq!(extra.len(), 0x2000);
            store
                .view()
                .write_obj(0x4433_2211_u32, GuestAddress(0x9000))
                .unwrap();
            assert_eq!(
                store.view().read_obj::<u32>(GuestAddress(0x9000)).unwrap(),
                0x4433_2211
            );
            // The view and the mapping share one host mapping.
            let region = store.view().find_region(GuestAddress(0x9000)).unwrap();
            assert_eq!(region.as_ptr(), extra.as_ptr());
            assert!(store.mapping(WindowId(3)).is_none());
            // Nothing is mapped between the windows.
            assert!(store.view().read_obj::<u8>(GuestAddress(0x7000)).is_err());
        }
    }

    #[test]
    fn layouts_that_do_not_fit_the_host_are_not_mapped() {
        let layout = MemoryLayout::boot(4 << 30).unwrap();
        let one_slot = HostCapabilities::fake(46, 1, 4096);
        let error = MemoryStore::materialize(layout, policy(BackingKind::SharedMemfd), &one_slot)
            .unwrap_err();
        assert!(matches!(
            error,
            MaterializeError::Policy(HostPolicyError::Memslots { count: 2, max: 1 })
        ));
        assert_eq!(
            crate::error::render_chain(&error),
            "memory layout does not fit the host: 2 boot windows need more than the 1 \
             memslots KVM provides"
        );
    }

    #[test]
    fn windows_at_the_top_of_the_address_space_are_refused() {
        let top = Window::new(
            ObjectId(0),
            Range::new(ObjectOffset(0), 0x1000).unwrap(),
            Gpa(u64::MAX - 0xfff),
            WindowOwner::Boot,
            WindowKind::BootRam,
        )
        .unwrap();
        let layout = MemoryLayout::new(
            [(ObjectId(0), Object { capacity: 0x1000 })],
            [(WindowId(0), top)],
        )
        .unwrap();
        // Without an address width, the host admits the whole space.
        let wide = HostCapabilities::fake(64, 32, 4096);
        for kind in KINDS {
            let error = MemoryStore::materialize(layout.clone(), policy(kind), &wide).unwrap_err();
            assert!(matches!(error, MaterializeError::EndsAtTop(WindowId(0))));
        }
    }
}
