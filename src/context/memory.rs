use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::{sync::Arc, vec::Vec};
use core::cmp;
use core::fmt::Debug;
use core::mem::ManuallyDrop;
use core::num::NonZeroUsize;
use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{RwLock, RwLockWriteGuard, Once, RwLockUpgradableGuard};
use syscall::{
    flag::MapFlags,
    error::*,
};
use rmm::Arch as _;

use crate::arch::paging::PAGE_SIZE;
use crate::common::{try_box_slice_new, try_new_vec_with_exact_size};
use crate::context::file::FileDescriptor;
use crate::memory::{Enomem, Frame, RaiiFrame, get_page_info, PageInfo};
use crate::paging::mapper::{Flusher, InactiveFlusher, PageFlushAll};
use crate::paging::{KernelMapper, Page, PageFlags, PageMapper, RmmA, TableKind, VirtualAddress};

pub const MMAP_MIN_DEFAULT: usize = PAGE_SIZE;

pub fn page_flags(flags: MapFlags) -> PageFlags<RmmA> {
    PageFlags::new()
        .user(true)
        .execute(flags.contains(MapFlags::PROT_EXEC))
        .write(flags.contains(MapFlags::PROT_WRITE))
        //TODO: PROT_READ
}
pub fn map_flags(page_flags: PageFlags<RmmA>) -> MapFlags {
    let mut flags = MapFlags::PROT_READ;
    if page_flags.has_write() { flags |= MapFlags::PROT_WRITE; }
    if page_flags.has_execute() { flags |= MapFlags::PROT_EXEC; }
    // TODO: MAP_SHARED/MAP_PRIVATE (requires that grants keep track of what they borrow and if
    // they borrow shared or CoW).
    flags
}

pub struct UnmapResult {
    pub file_desc: Option<GrantFileRef>,
}
impl Drop for UnmapResult {
    fn drop(&mut self) {
        if let Some(fd) = self.file_desc.take() {
            let _ = fd.desc.close();
        }
    }
}

pub fn new_addrspace() -> Result<Arc<RwLock<AddrSpace>>> {
    Arc::try_new(RwLock::new(AddrSpace::new()?)).map_err(|_| Error::new(ENOMEM))
}

#[derive(Debug)]
pub struct AddrSpace {
    pub table: Table,
    pub grants: UserGrants,
    /// Lowest offset for mmap invocations where the user has not already specified the offset
    /// (using MAP_FIXED/MAP_FIXED_NOREPLACE). Cf. Linux's `/proc/sys/vm/mmap_min_addr`, but with
    /// the exception that we have a memory safe kernel which doesn't have to protect itself
    /// against null pointers, so fixed mmaps to address zero are still allowed.
    pub mmap_min: usize,
}
impl AddrSpace {
    pub fn current() -> Result<Arc<RwLock<Self>>> {
        Ok(Arc::clone(super::current()?.read().addr_space()?))
    }

    /// Attempt to clone an existing address space so that all mappings are copied (CoW).
    pub fn try_clone(&mut self, self_arc: Arc<RwLock<Self>>) -> Result<Arc<RwLock<Self>>> {
        let mut new = new_addrspace()?;

        let new_guard = Arc::get_mut(&mut new)
            .expect("expected new address space Arc not to be aliased")
            .get_mut();

        let this_mapper = &mut self.table.utable;
        let new_mapper = &mut new_guard.table.utable;
        let mut this_flusher = PageFlushAll::new();

        for (grant_base, grant_info) in self.grants.iter() {
            let new_grant = match grant_info.provider {
                Provider::PhysBorrowed { base } => Grant::physmap(base.clone(), PageSpan::new(grant_base, grant_info.page_count), grant_info.flags, new_mapper, ())?,
                Provider::Allocated => Grant::cow(Arc::clone(&self_arc), grant_base, grant_base, grant_info.page_count, grant_info.flags, this_mapper, new_mapper, &mut this_flusher, ())?,

                // MAP_SHARED grants are retained by reference, across address space clones (across
                // forks on monolithic kernels).
                Provider::External { ref address_space, src_base } => Grant::borrow_grant(Arc::clone(&address_space), grant_base, grant_base, grant_info, new_mapper, (), false)?,

                Provider::Fmap { ref desc } => todo!(),
            };

            new_guard.grants.insert(new_grant);
        }
        Ok(new)
    }
    pub fn new() -> Result<Self> {
        Ok(Self {
            grants: UserGrants::new(),
            table: setup_new_utable()?,
            mmap_min: MMAP_MIN_DEFAULT,
        })
    }
    pub fn is_current(&self) -> bool {
        self.table.utable.is_current()
    }
    pub fn mprotect(&mut self, requested_span: PageSpan, flags: MapFlags) -> Result<()> {
        let (mut active, mut inactive);
        let mut flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<RmmA>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<RmmA>
        };
        let mapper = &mut self.table.utable;

        // TODO: Remove allocation (might require BTreeMap::set_key or interior mutability).
        let regions = self.grants.conflicts(requested_span).map(|(base, info)| PageSpan::new(base, info.page_count)).collect::<Vec<_>>();

        for grant_span in regions {
            let grant = self.grants.remove(grant_span.base).expect("grant cannot magically disappear while we hold the lock!");
            //log::info!("Mprotecting {:#?} to {:#?} in {:#?}", grant, flags, grant_span);
            let intersection = grant_span.intersection(requested_span);

            let (before, mut grant, after) = grant.extract(intersection).expect("failed to extract grant");
            //log::info!("Sliced into\n\n{:#?}\n\n{:#?}\n\n{:#?}", before, grant, after);

            if let Some(before) = before { self.grants.insert(before); }
            if let Some(after) = after { self.grants.insert(after); }

            if !grant.info.can_have_flags(flags) {
                self.grants.insert(grant);
                return Err(Error::new(EACCES));
            }

            let new_flags = grant.info.flags()
                // TODO: Require a capability in order to map executable memory?
                .execute(flags.contains(MapFlags::PROT_EXEC))
                .write(flags.contains(MapFlags::PROT_WRITE));

            // TODO: Allow enabling/disabling read access on architectures which allow it. On
            // x86_64 with protection keys (although only enforced by userspace), and AArch64 (I
            // think), execute-only memory is also supported.

            grant.remap(mapper, &mut flusher, new_flags);
            //log::info!("Mprotect grant became {:#?}", grant);
            self.grants.insert(grant);
        }
        Ok(())
    }
    pub fn munmap(mut self: RwLockWriteGuard<'_, Self>, requested_span: PageSpan) {
        let mut notify_files = Vec::new();

        let mut flusher = PageFlushAll::new();

        // TODO: Allocating may even be wrong!
        let conflicting: Vec<PageSpan> = self.grants.conflicts(requested_span).map(|(base, info)| PageSpan::new(base, info.page_count)).collect();

        for conflict in conflicting {
            let grant = self.grants.remove(conflict.base).expect("conflicting region didn't exist");
            let intersection = conflict.intersection(requested_span);
            let (before, mut grant, after) = grant.extract(intersection).expect("conflicting region shared no common parts");

            // Notify scheme that holds grant
            if let Provider::Fmap { ref desc } = grant.info.provider {
                // TODO: Remove clone
                notify_files.push((desc.clone(), intersection));
            }

            // Keep untouched regions
            if let Some(before) = before {
                self.grants.insert(before);
            }
            if let Some(after) = after {
                self.grants.insert(after);
            }

            // Remove irrelevant region
            grant.unmap(&mut self.table.utable, &mut flusher);
        }
        drop(self);

        for (file_ref, intersection) in notify_files {
            let scheme_id = { file_ref.desc.description.read().scheme };

            let scheme = match crate::scheme::schemes().get(scheme_id) {
                Some(scheme) => Arc::clone(scheme),
                // One could argue that EBADFD could be returned here, but we have already unmapped
                // the memory.
                None => continue,
            };
            // Same here, we don't really care about errors when schemes respond to unmap events.
            // The caller wants the memory to be unmapped, period. When already unmapped, what
            // would we do with error codes anyway?
            let _ = scheme.funmap(intersection.base.start_address().data(), intersection.count * PAGE_SIZE);

            let _ = file_ref.desc.close();
        }
    }
    pub fn mmap(&mut self, page: Option<Page>, page_count: NonZeroUsize, flags: MapFlags, map: impl FnOnce(Page, PageFlags<RmmA>, &mut PageMapper, &mut dyn Flusher<RmmA>) -> Result<Grant>) -> Result<Page> {
        self.mmap_multiple(page, page_count, flags, move |page, flags, mapper, flusher| Ok(Some(map(page, flags, mapper, flusher)?)))
    }
    pub fn mmap_multiple<I: IntoIterator<Item = Grant>>(&mut self, page: Option<Page>, page_count: NonZeroUsize, flags: MapFlags, map: impl FnOnce(Page, PageFlags<RmmA>, &mut PageMapper, &mut dyn Flusher<RmmA>) -> Result<I>) -> Result<Page> {
        // Finally, the end of all "T0DO: Abstract with other grant creation"!
        let selected_span = self.grants.find_free_at(self.mmap_min, page, page_count.get(), flags)?;

        // TODO: Threads share address spaces, so not only the inactive flusher should be sending
        // out IPIs. IPIs will only be sent when downgrading mappings (i.e. when a stale TLB entry
        // will not be corrected by a page fault), and will furthermore require proper
        // synchronization.
        let (mut active, mut inactive);
        let flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<RmmA>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<RmmA>
        };

        let iter = map(selected_span.base, page_flags(flags), &mut self.table.utable, flusher)?;
        for grant in iter {
            self.grants.insert(grant);
        }

        Ok(selected_span.base)
    }
}

#[derive(Debug)]
pub struct UserGrants {
    inner: BTreeMap<Page, GrantInfo>,
    holes: BTreeMap<VirtualAddress, usize>,
    // TODO: Would an additional map ordered by (size,start) to allow for O(log n) allocations be
    // beneficial?

    //TODO: technically VirtualAddress is from a scheme's context!
    pub funmap: BTreeMap<Page, (usize, Page)>,
}

#[derive(Clone, Copy)]
pub struct PageSpan {
    pub base: Page,
    pub count: usize,
}
impl PageSpan {
    pub fn new(base: Page, count: usize) -> Self {
        Self { base, count }
    }
    pub fn validate_nonempty(address: VirtualAddress, size: usize) -> Option<Self> {
        Self::validate(address, size).filter(|this| !this.is_empty())
    }
    pub fn validate(address: VirtualAddress, size: usize) -> Option<Self> {
        if address.data() % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 { return None; }
        if address.data().saturating_add(size) > crate::USER_END_OFFSET { return None; }

        Some(Self::new(Page::containing_address(address), size / PAGE_SIZE))
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn intersection(&self, with: PageSpan) -> PageSpan {
        Self::between(
            cmp::max(self.base, with.base),
            cmp::min(self.end(), with.end()),
        )
    }
    pub fn intersects(&self, with: PageSpan) -> bool {
        !self.intersection(with).is_empty()
    }
    pub fn contains(&self, page: Page) -> bool {
        self.intersects(Self::new(page, 1))
    }
    pub fn slice(&self, inner_span: PageSpan) -> (Option<PageSpan>, PageSpan, Option<PageSpan>) {
        (self.before(inner_span), inner_span, self.after(inner_span))
    }
    pub fn pages(self) -> impl Iterator<Item = Page> {
        (0..self.count).map(move |i| self.base.next_by(i))
    }

    pub fn end(&self) -> Page {
        self.base.next_by(self.count)
    }

    /// Returns the span from the start of self until the start of the specified span.
    pub fn before(self, span: Self) -> Option<Self> {
        assert!(self.base <= span.base);
        Some(Self::between(
            self.base,
            span.base,
        )).filter(|reg| !reg.is_empty())
    }

    /// Returns the span from the end of the given span until the end of self.
    pub fn after(self, span: Self) -> Option<Self> {
        assert!(span.end() <= self.end());
        Some(Self::between(
            span.end(),
            self.end(),
        )).filter(|reg| !reg.is_empty())
    }
    /// Returns the span between two pages, `[start, end)`, truncating to zero if end < start.
    pub fn between(start: Page, end: Page) -> Self {
        Self::new(
            start,
            end.start_address().data().saturating_sub(start.start_address().data()) / PAGE_SIZE,
        )
    }

    pub fn rebase(self, new_base: Self, page: Page) -> Page {
        let offset = page.offset_from(self.base);
        new_base.base.next_by(offset)
    }
}

impl Default for UserGrants {
    fn default() -> Self {
        Self::new()
    }
}
impl Debug for PageSpan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[{:p}:{:p}, {} pages]", self.base.start_address().data() as *const u8, self.base.start_address().add(self.count * PAGE_SIZE - 1).data() as *const u8, self.count)
    }
}

impl UserGrants {
    pub fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
            holes: core::iter::once((VirtualAddress::new(0), crate::USER_END_OFFSET)).collect::<BTreeMap<_, _>>(),
            funmap: BTreeMap::new(),
        }
    }
    /// Returns the grant, if any, which occupies the specified page
    pub fn contains(&self, page: Page) -> Option<(Page, &GrantInfo)> {
        self.inner
            .range(..=page)
            .next_back()
            .filter(|(base, info)| (**base..base.next_by(info.page_count)).contains(&page))
            .map(|(base, info)| (*base, info))
    }
    // TODO: Deduplicate code?
    pub fn contains_mut(&mut self, page: Page) -> Option<(Page, &mut GrantInfo)> {
        self.inner
            .range_mut(..=page)
            .next_back()
            .filter(|(base, info)| (**base..base.next_by(info.page_count)).contains(&page))
            .map(|(base, info)| (*base, info))
    }
    /// Returns an iterator over all grants that occupy some part of the
    /// requested region
    pub fn conflicts(&self, span: PageSpan) -> impl Iterator<Item = (Page, &'_ GrantInfo)> + '_ {
        let start = self.contains(span.base);

        // If there is a grant that contains the base page, start searching at the base of that
        // grant, rather than the requested base here.
        let start_span = start.map(|(base, info)| PageSpan::new(base, info.page_count)).unwrap_or(span);

        self
            .inner
            .range(start_span.base..)
            .take_while(move |(base, info)| PageSpan::new(**base, info.page_count).intersects(span))
            .map(|(base, info)| (*base, info))
    }
    /// Return a free region with the specified size
    // TODO: Alignment (x86_64: 4 KiB, 2 MiB, or 1 GiB).
    pub fn find_free(&self, min: usize, page_count: usize) -> Option<PageSpan> {
        // Get first available hole, but do reserve the page starting from zero as most compiled
        // languages cannot handle null pointers safely even if they point to valid memory. If an
        // application absolutely needs to map the 0th page, they will have to do so explicitly via
        // MAP_FIXED/MAP_FIXED_NOREPLACE.
        // TODO: Allow explicitly allocating guard pages? Perhaps using mprotect or mmap with
        // PROT_NONE?

        let (hole_start, _hole_size) = self.holes.iter()
            .skip_while(|(hole_offset, hole_size)| hole_offset.data() + **hole_size <= min)
            .find(|(hole_offset, hole_size)| {
                let avail_size = if hole_offset.data() <= min && min <= hole_offset.data() + **hole_size {
                    **hole_size - (min - hole_offset.data())
                } else {
                    **hole_size
                };
                page_count * PAGE_SIZE <= avail_size
            })?;
        // Create new region
        Some(PageSpan::new(Page::containing_address(VirtualAddress::new(cmp::max(hole_start.data(), min))), page_count))
    }
    /// Return a free region, respecting the user's hinted address and flags. Address may be null.
    pub fn find_free_at(&mut self, min: usize, base: Option<Page>, page_count: usize, flags: MapFlags) -> Result<PageSpan> {
        let Some(requested_base) = base else {
            // Free hands!
            return self.find_free(min, page_count).ok_or(Error::new(ENOMEM));
        };

        // The user wished to have this region...
        let requested_span = PageSpan::new(requested_base, page_count);

        if let Some(_grant) = self.conflicts(requested_span).next() {
            // ... but it already exists

            if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                return Err(Error::new(EEXIST));
            }
            if flags.contains(MapFlags::MAP_FIXED) {
                // TODO: find_free_at -> Result<(PageSpan, needs_to_unmap: PageSpan)>
                return Err(Error::new(EOPNOTSUPP));
            } else {
                // TODO: Find grant close to requested address?
                return self.find_free(min, page_count).ok_or(Error::new(ENOMEM));
            }
        }

        Ok(requested_span)
    }
    fn reserve(&mut self, base: Page, page_count: usize) {
        let start_address = base.start_address();
        let size = page_count * PAGE_SIZE;
        let end_address = base.start_address().add(size);

        let previous_hole = self.holes.range_mut(..start_address).next_back();

        if let Some((hole_offset, hole_size)) = previous_hole {
            let prev_hole_end = hole_offset.data() + *hole_size;

            // Note that prev_hole_end cannot exactly equal start_address, since that would imply
            // there is another grant at that position already, as it would otherwise have been
            // larger.

            if prev_hole_end > start_address.data() {
                // hole_offset must be below (but never equal to) the start address due to the
                // `..start_address()` limit; hence, all we have to do is to shrink the
                // previous offset.
                *hole_size = start_address.data() - hole_offset.data();
            }
            if prev_hole_end > end_address.data() {
                // The grant is splitting this hole in two, so insert the new one at the end.
                self.holes.insert(end_address, prev_hole_end - end_address.data());
            }
        }

        // Next hole
        if let Some(hole_size) = self.holes.remove(&start_address) {
            let remainder = hole_size - size;
            if remainder > 0 {
                self.holes.insert(end_address, remainder);
            }
        }
    }
    fn unreserve(holes: &mut BTreeMap<VirtualAddress, usize>, base: Page, page_count: usize) {
        // TODO
        let start_address = base.start_address();
        let size = page_count * PAGE_SIZE;
        let end_address = base.start_address().add(size);

        // The size of any possible hole directly after the to-be-freed region.
        let exactly_after_size = holes.remove(&end_address);

        // There was a range that began exactly prior to the to-be-freed region, so simply
        // increment the size such that it occupies the grant too. If in addition there was a grant
        // directly after the grant, include it too in the size.
        if let Some((hole_offset, hole_size)) = holes.range_mut(..start_address).next_back().filter(|(offset, size)| offset.data() + **size == start_address.data()) {
            *hole_size = end_address.data() - hole_offset.data() + exactly_after_size.unwrap_or(0);
        } else {
            // There was no free region directly before the to-be-freed region, however will
            // now unconditionally insert a new free region where the grant was, and add that extra
            // size if there was something after it.
            holes.insert(start_address, size + exactly_after_size.unwrap_or(0));
        }
    }
    pub fn insert(&mut self, grant: Grant) {
        assert!(self.conflicts(PageSpan::new(grant.base, grant.info.page_count)).next().is_none());
        self.reserve(grant.base, grant.info.page_count);

        // FIXME: This currently causes issues, mostly caused by old code that unmaps only based on
        // offsets. For instance, the scheme code does not specify any length, and would thus unmap
        // memory outside of what it intended to.

        /*
        let before_region = self.inner
            .range(..grant.base).next_back()
            .filter(|(base, info)| base.next_by(info.page_count) == grant.base && info.can_be_merged_if_adjacent(&grant.info)).map(|(base, info)| (*base, info.page_count));

        let after_region = self.inner
            .range(grant.span().end()..).next()
            .filter(|(base, info)| **base == grant.base.next_by(grant.info.page_count) && info.can_be_merged_if_adjacent(&grant.info)).map(|(base, info)| (*base, info.page_count));

        if let Some((before_base, before_page_count)) = before_region {
            grant.base = before_base;
            grant.info.page_count += before_page_count;

            core::mem::forget(self.inner.remove(&before_base));
        }
        if let Some((after_base, after_page_count)) = after_region {
            grant.info.page_count += after_page_count;

            core::mem::forget(self.inner.remove(&after_base));
        }
        */

        self.inner.insert(grant.base, grant.info);
    }
    pub fn remove(&mut self, base: Page) -> Option<Grant> {
        let info = self.inner.remove(&base)?;
        Self::unreserve(&mut self.holes, base, info.page_count);
        Some(Grant { base, info })
    }
    pub fn iter(&self) -> impl Iterator<Item = (Page, &GrantInfo)> + '_ {
        self.inner.iter().map(|(base, info)| (*base, info))
    }
    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
    pub fn into_iter(self) -> impl Iterator<Item = Grant> {
        self.inner.into_iter().map(|(base, info)| Grant { base, info })
    }
}

#[derive(Debug)]
pub struct GrantInfo {
    page_count: usize,
    flags: PageFlags<RmmA>,
    // TODO: Rename to unmapped?
    mapped: bool,
    pub(crate) provider: Provider,
}

#[derive(Debug)]
pub enum Provider {
    /// The grant was initialized with (lazy) zeroed memory, and any changes will make it owned by
    /// the frame allocator.
    Allocated,
    /// The grant is not owned, but borrowed from physical memory frames that do not belong to the
    /// frame allocator.
    PhysBorrowed { base: Frame },
    /// The memory is borrowed directly from another address space.
    ///
    /// All grants in the specified range must be of type Allocated.
    External { address_space: Arc<RwLock<AddrSpace>>, src_base: Page },
    /// The memory is borrowed from another address space, but managed by a scheme via fmap.
    // TODO: This is probably a very heavy way to keep track of fmap'd files, perhaps move to the
    // ~~context~~ address space?
    // TODO: mmap CoW
    Fmap { desc: GrantFileRef },
}

#[derive(Debug)]
pub struct Grant {
    pub(crate) base: Page,
    pub(crate) info: GrantInfo,
}

#[derive(Clone, Debug)]
pub struct GrantFileRef {
    pub desc: FileDescriptor,
    pub offset: usize,
    // TODO: Can the flags maybe be stored together with the page flags. Should some flags be kept,
    // and others discarded when re-fmapping on clone?
    pub flags: MapFlags,
}

static THE_ZEROED_FRAME: Once<Frame> = Once::new();

impl Grant {
    // TODO: PageCount newtype, to avoid confusion between bytes and pages?

    pub fn physmap(phys: Frame, span: PageSpan, flags: PageFlags<RmmA>, mapper: &mut PageMapper, mut flusher: impl Flusher<RmmA>) -> Result<Grant> {
        Ok(Grant {
            base: span.base,
            info: GrantInfo {
                page_count: span.count,
                flags,
                mapped: true,
                provider: Provider::PhysBorrowed { base: phys },
            },
        })
    }
    pub fn zeroed(span: PageSpan, flags: PageFlags<RmmA>, mapper: &mut PageMapper, mut flusher: impl Flusher<RmmA>) -> Result<Grant, Enomem> {
        //let the_frame = THE_ZEROED_FRAME.get().expect("expected the zeroed frame to be available").start_address();

        // TODO: O(n) readonly map with zeroed page, or O(1) no-op and then lazily map?
        // TODO: Use flush_all after a certain number of pages, otherwise no

        /*
        for page in span.pages() {
            // Good thing with lazy page fault handlers, is that if we fail due to ENOMEM here, we
            // can continue and let the process face the OOM killer later.
            unsafe {
                let Some(result) = mapper.map_phys(page.start_address(), the_frame.start_address(), flags.write(false)) else {
                    break;
                };
                flusher.consume(result);
            }
        }
        */

        Ok(Grant {
            base: span.base,
            info: GrantInfo {
                page_count: span.count,
                flags,
                mapped: true,
                provider: Provider::Allocated,
            },
        })
    }

    // XXX: borrow_grant is needed because of the borrow checker (iterator invalidation), maybe
    // borrow_grant/borrow can be abstracted somehow?
    pub fn borrow_grant(src_address_space_lock: Arc<RwLock<AddrSpace>>, src_base: Page, dst_base: Page, src_info: &GrantInfo, mapper: &mut PageMapper, dst_flusher: impl Flusher<RmmA>, eager: bool) -> Result<Grant, Enomem> {
        Ok(Grant {
            base: dst_base,
            info: GrantInfo {
                page_count: src_info.page_count,
                flags: src_info.flags,
                mapped: true,
                provider: Provider::External {
                    src_base,
                    address_space: src_address_space_lock,
                }
            },
        })
    }

    // TODO: Do not return Vec, return an iterator perhaps? Referencing the source address space?

    /// Borrow all pages in the range `[src_base, src_base+page_count)` from `src_address_space`,
    /// mapping them into `[dst_base, dst_base+page_count)`. The destination pages will lazily read
    /// the page tables of the source pages, but once present in the destination address space,
    /// pages that are unmaped or moved will not be made visible to the destination address space.
    pub fn borrow(
        src_address_space_lock: Arc<RwLock<AddrSpace>>,
        src_address_space: &AddrSpace,
        src_base: Page,
        dst_base: Page,
        page_count: usize,
        flags: PageFlags<RmmA>,
        dst_mapper: &mut PageMapper,
        dst_flusher: impl Flusher<RmmA>,
        eager: bool,
    ) -> Result<Vec<Grant>, Enomem> {
        /*
        if eager {
            for page in PageSpan::new(src_base, page_count) {
                // ...
            }
        }
        */

        let mut dst_grants = Vec::with_capacity(1);

        let src_span = PageSpan::new(src_base, page_count);

        for (src_base, src_grant) in src_address_space.grants.conflicts(src_span) {
            let grant_span = PageSpan::new(src_base, src_grant.page_count);

            let common_span = src_span.intersection(grant_span);
            let offset_from_src_base = common_span.base.offset_from(src_base);

            dst_grants.push(Grant {
                base: dst_base.next_by(offset_from_src_base),
                info: GrantInfo {
                    page_count: common_span.count,
                    flags,
                    mapped: true,
                    provider: match src_grant.provider {
                        Provider::Allocated => Provider::External {
                            src_base,
                            address_space: Arc::clone(&src_address_space_lock),
                        },
                        Provider::PhysBorrowed { base: src_phys_base } => Provider::PhysBorrowed {
                            base: src_phys_base.next_by(offset_from_src_base),
                        },
                        Provider::Fmap { .. } => todo!(),
                        Provider::External { ref address_space, src_base } => Provider::External { address_space: Arc::clone(address_space), src_base },
                    }
                },
            });
        }

        Ok(dst_grants)
    }
    // TODO: This is limited to one grant. Should it be (if some magic new proc: API is introduced)?
    pub fn cow(
        src_address_space_lock: Arc<RwLock<AddrSpace>>,
        src_base: Page,
        dst_base: Page,
        page_count: usize,
        flags: PageFlags<RmmA>,
        src_mapper: &mut PageMapper,
        dst_mapper: &mut PageMapper,
        mut src_flusher: impl Flusher<RmmA>,
        mut dst_flusher: impl Flusher<RmmA>,
    ) -> Result<Grant, Enomem> {
        // TODO: Page table iterator
        for page_idx in 0..page_count {
            let src_page = src_base.next_by(page_idx);
            let dst_page = dst_base.next_by(page_idx).start_address();

            let Some((_old_flags, src_phys, flush)) = (unsafe { src_mapper.remap_with(src_page.start_address(), |flags| flags.write(false)) }) else {
                // Page is not mapped, let the page fault handler take care of that (initializing
                // it to zero).
                //
                // TODO: If eager, allocate zeroed page if writable, or use *the* zeroed page (also
                // for read-only)?
                continue;
            };
            let src_frame = Frame::containing_address(src_phys);

            let src_page_info = get_page_info(src_frame).expect("allocated page was not present in the global page array");
            src_page_info.add_ref(true);

            let Some(map_result) = (unsafe { dst_mapper.map_phys(dst_page, src_frame.start_address(), flags.write(false)) }) else {
                break;
            };

            dst_flusher.consume(map_result);
        }

        Ok(Grant {
            base: dst_base,
            info: GrantInfo {
                page_count,
                flags,
                mapped: true,
                provider: Provider::Allocated,
            },
        })
    }
    pub fn transfer(mut src_grant: Grant, dst_base: Page, src_mapper: &mut PageMapper, dst_mapper: &mut PageMapper, src_flusher: impl Flusher<RmmA>, dst_flusher: impl Flusher<RmmA>) -> Result<Grant> {
        todo!()
        /*
        assert!(core::mem::replace(&mut src_grant.info.mapped, false));
        let desc_opt = src_grant.info.desc_opt.take();

        Self::copy_inner(src_grant.base, dst_base, src_grant.info.page_count, src_grant.info.flags(), desc_opt, src_mapper, dst_mapper, src_flusher, dst_flusher, src_grant.info.owned, true).map_err(Into::into)
            */
    }

    pub fn remap(&mut self, mapper: &mut PageMapper, mut flusher: impl Flusher<RmmA>, flags: PageFlags<RmmA>) {
        assert!(self.info.mapped);

        for page in self.span().pages() {
            // TODO: PageMapper is unsafe because it can be used to modify kernel memory. Add a
            // subset/wrapper that is safe but only for user mappings.
            unsafe {
                // Lazy mappings don't require remapping, as info.flags will be updated.
                let Some(result) = mapper.remap(page.start_address(), flags) else {
                    continue;
                };
                //log::info!("Remapped page {:?} (frame {:?})", page, Frame::containing_address(mapper.translate(page.start_address()).unwrap().0));
                flusher.consume(result);
            }
        }

        self.info.flags = flags;
    }
    pub fn unmap(mut self, mapper: &mut PageMapper, mut flusher: impl Flusher<RmmA>) -> UnmapResult {
        assert!(self.info.mapped);

        for page in self.span().pages() {
            // Lazy mappings do not need to be unmapped.
            let Some((phys, _, flush)) = (unsafe { mapper.unmap_phys(page.start_address(), true) }) else {
                continue;
            };
            let frame = Frame::containing_address(phys);

            match self.info.provider {
                Provider::Allocated | Provider::External { .. } => {
                    /*get_page_info(frame)
                        .expect("allocated frame did not have an associated PageInfo")
                        .remove_ref(is_cow);*/
                }
                _ => (),
            }


            flusher.consume(flush);
        }

        self.info.mapped = false;

        UnmapResult {
            file_desc: if let Provider::Fmap { ref desc } = self.info.provider {
                // TODO: Don't clone
                Some(desc.clone())
            } else {
                None
            }
        }
    }

    /// Extract out a region into a separate grant. The return value is as
    /// follows: (before, new split, after). Before and after may be `None`,
    /// which occurs when the split off region is at the start or end of the
    /// page respectively.
    ///
    /// # Panics
    ///
    /// Panics if the start or end addresses of the region is not aligned to the
    /// page size. To round up the size to the nearest page size, use `.round()`
    /// on the region.
    ///
    /// Also panics if the given region isn't completely contained within the
    /// grant. Use `grant.intersect` to find a sub-region that works.
    pub fn span(&self) -> PageSpan {
        PageSpan::new(self.base, self.info.page_count)
    }
    pub fn extract(mut self, span: PageSpan) -> Option<(Option<Grant>, Grant, Option<Grant>)> {
        let (before_span, this_span, after_span) = self.span().slice(span);

        let before_grant = before_span.map(|span| Grant {
            base: span.base,
            info: GrantInfo {
                flags: self.info.flags,
                mapped: self.info.mapped,
                page_count: span.count,
                provider: match self.info.provider {
                    Provider::Fmap { .. } => todo!(),
                    Provider::External { ref address_space, src_base } => Provider::External {
                        address_space: Arc::clone(address_space),
                        src_base,
                    },
                    Provider::Allocated { .. } => Provider::Allocated,
                    Provider::PhysBorrowed { ref base } => Provider::PhysBorrowed { base: base.clone() },
                }
            },
        });

        match self.info.provider {
            Provider::Fmap { .. } => todo!(),

            Provider::PhysBorrowed { ref mut base } => *base = base.next_by(before_grant.as_ref().map_or(0, |g| g.info.page_count)),
            Provider::Allocated | Provider::External { .. } => (),
        }


        let after_grant = after_span.map(|span| Grant {
            base: span.base,
            info: GrantInfo {
                flags: self.info.flags,
                mapped: self.info.mapped,
                page_count: span.count,
                provider: match self.info.provider {
                    Provider::Fmap { .. } => todo!(),

                    Provider::Allocated => Provider::Allocated,
                    Provider::External { ref address_space, src_base } => Provider::External {
                        address_space: Arc::clone(address_space),
                        src_base,
                    },

                    Provider::PhysBorrowed { base } => Provider::PhysBorrowed { base: base.next_by(this_span.count) },
                }
            },
        });

        self.base = this_span.base;
        self.info.page_count = this_span.count;

        Some((before_grant, self, after_grant))
    }
}
impl GrantInfo {
    pub fn flags(&self) -> PageFlags<RmmA> {
        self.flags
    }
    pub fn page_count(&self) -> usize {
        self.page_count
    }
    pub fn can_have_flags(&self, flags: MapFlags) -> bool {
        // TODO: read
        let is_downgrade = (self.flags.has_write() || !flags.contains(MapFlags::PROT_WRITE)) && (self.flags.has_execute() || !flags.contains(MapFlags::PROT_EXEC));

        match self.provider {
            Provider::Allocated => true,
            _ => is_downgrade,
        }
    }

    pub fn can_be_merged_if_adjacent(&self, with: &Self) -> bool {
        if self.mapped != with.mapped || self.flags.data() != with.flags.data() {
            return false;
        }

        match (&self.provider, &with.provider) {
            (Provider::Fmap { .. }, Provider::Fmap { .. }) => todo!(),
            //(Provider::PhysBorrowed { base: ref lhs }, Provider::PhysBorrowed { base: ref rhs }) => lhs.next_by(self.page_count) == rhs.clone(),
            // TODO: Add merge function that merges the page array.
            //(Provider::Allocated { .. }, Provider::Allocated { .. }) => true,
            //(Provider::External { address_space: ref lhs_space, src_base: ref lhs_base, cow: lhs_cow, .. }, Provider::External { address_space: ref rhs_space, src_base: ref rhs_base, cow: rhs_cow, .. }) => Arc::ptr_eq(lhs_space, rhs_space) && lhs_cow == rhs_cow && lhs_base.next_by(self.page_count) == rhs_base.clone(),

            _ => false,
        }
    }
}

impl Drop for GrantInfo {
    fn drop(&mut self) {
        // XXX: This will not show the address...
        assert!(!self.mapped, "Grant dropped while still mapped: {:#x?}", self);
    }
}

pub const DANGLING: usize = 1 << (usize::BITS - 2);

#[derive(Debug)]
pub struct Table {
    pub utable: PageMapper,
}

impl Drop for Table {
    fn drop(&mut self) {
        if self.utable.is_current() {
            // TODO: Do not flush (we immediately context switch after exit(), what else is there
            // to do?). Instead, we can garbage-collect such page tables in the idle kernel context
            // before it waits for interrupts. Or maybe not, depends on what future benchmarks will
            // indicate.
            unsafe {
                RmmA::set_table(TableKind::User, super::empty_cr3());
            }
        }
        crate::memory::deallocate_frames(Frame::containing_address(self.utable.table().phys()), 1);
    }
}

/// Allocates a new empty utable
#[cfg(target_arch = "aarch64")]
pub fn setup_new_utable() -> Result<Table> {
    let utable = unsafe { PageMapper::create(TableKind::User, crate::rmm::FRAME_ALLOCATOR).ok_or(Error::new(ENOMEM))? };

    Ok(Table {
        utable,
    })
}

/// Allocates a new identically mapped ktable and empty utable (same memory on x86)
#[cfg(target_arch = "x86")]
pub fn setup_new_utable() -> Result<Table> {
    let mut utable = unsafe { PageMapper::create(TableKind::User, crate::rmm::FRAME_ALLOCATOR).ok_or(Error::new(ENOMEM))? };

    {
        let active_ktable = KernelMapper::lock();

        let mut copy_mapping = |p4_no| unsafe {
            let entry = active_ktable.table().entry(p4_no)
                .unwrap_or_else(|| panic!("expected kernel PML {} to be mapped", p4_no));

            utable.table().set_entry(p4_no, entry)
        };

        // Copy higher half (kernel) mappings
        for i in 512..1024 {
            copy_mapping(i);
        }
    }

    Ok(Table {
        utable,
    })
}

/// Allocates a new identically mapped ktable and empty utable (same memory on x86_64).
#[cfg(target_arch = "x86_64")]
pub fn setup_new_utable() -> Result<Table> {
    let utable = unsafe { PageMapper::create(TableKind::User, crate::rmm::FRAME_ALLOCATOR).ok_or(Error::new(ENOMEM))? };

    {
        let active_ktable = KernelMapper::lock();

        let copy_mapping = |p4_no| unsafe {
            let entry = active_ktable.table().entry(p4_no)
                .unwrap_or_else(|| panic!("expected kernel PML {} to be mapped", p4_no));

            utable.table().set_entry(p4_no, entry)
        };
        // TODO: Just copy all 256 mappings? Or copy KERNEL_PML4+KERNEL_PERCPU_PML4 (needed for
        // paranoid ISRs which can occur anywhere; we don't want interrupts to triple fault!) and
        // map lazily via page faults in the kernel.

        // Copy kernel image mapping
        copy_mapping(crate::KERNEL_PML4);

        // Copy kernel heap mapping
        copy_mapping(crate::KERNEL_HEAP_PML4);

        // Copy physmap mapping
        copy_mapping(crate::PHYS_PML4);
    }

    Ok(Table {
        utable,
    })
}
#[derive(Clone, Copy, PartialEq)]
pub enum AccessMode {
    Read,
    Write,
    InstrFetch,
}

pub enum PfError {
    Segv,
    Oom,
    NonfatalInternalError,
}

fn cow(dst_mapper: &mut PageMapper, page: Page, old_frame: Frame, info: &PageInfo, page_flags: PageFlags<RmmA>) -> Result<Frame, PfError> {
    let new_frame = init_frame()?;

    unsafe { copy_frame_to_frame_directly(new_frame, old_frame); }

    info.remove_ref(true);

    Ok(new_frame)
}

fn init_frame() -> Result<Frame, PfError> {
    let new_frame = crate::memory::allocate_frames(1).ok_or(PfError::Oom)?;
    let page_info = get_page_info(new_frame).expect("all allocated frames need an associated page info");
    page_info.refcount.store(1, Ordering::Relaxed);
    page_info.cow_refcount.store(1, Ordering::Relaxed);

    Ok(new_frame)
}

fn map_zeroed(mapper: &mut PageMapper, page: Page, page_flags: PageFlags<RmmA>, _writable: bool) -> Result<Frame, PfError> {
    let new_frame = init_frame()?;

    unsafe {
        mapper.map_phys(page.start_address(), new_frame.start_address(), page_flags).ok_or(PfError::Oom)?.flush();
    }

    Ok(new_frame)
}

pub unsafe fn copy_frame_to_frame_directly(dst: Frame, src: Frame) {
    // Optimized exact-page-size copy function?
    let dst = unsafe { RmmA::phys_to_virt(dst.start_address()).data() as *mut u8 };
    let src = unsafe { RmmA::phys_to_virt(src.start_address()).data() as *const u8 };

    unsafe {
        dst.copy_from_nonoverlapping(src, PAGE_SIZE);
    }
}

pub fn try_correcting_page_tables(faulting_page: Page, access: AccessMode) -> Result<(), PfError> {
    let Ok(addr_space) = AddrSpace::current() else {
        log::warn!("User page fault without address space being set.");
        return Err(PfError::Segv);
    };

    let mut addr_space = addr_space.write();
    let addr_space = &mut *addr_space;

    let Some((grant_base, grant_info)) = addr_space.grants.contains(faulting_page) else {
        return Err(PfError::Segv);
    };

    let pages_from_grant_start = faulting_page.offset_from(grant_base);

    let grant_flags = grant_info.flags();
    match access {
        // TODO: has_read
        AccessMode::Read => (),

        AccessMode::Write if !grant_flags.has_write() => return Err(PfError::Segv),
        AccessMode::InstrFetch if !grant_flags.has_execute() => return Err(PfError::Segv),

        _ => (),
    }

    // By now, the memory at the faulting page is actually valid, but simply not yet mapped, either
    // at all, or with the required flags.

    let faulting_frame_opt = addr_space.table.utable
        .translate(faulting_page.start_address())
        .map(|(phys, _page_flags)| Frame::containing_address(phys));
    let faulting_pageinfo_opt = faulting_frame_opt.map(|frame| (frame, get_page_info(frame)));

    // TODO: Readahead!
    //
    // TODO: Aligned readahead? AMD Zen3+ CPUs can smash 4 4k pages that are 16k-aligned, into a
    // single TLB entry, thus emulating 16k pages albeit with higher page table overhead. With the
    // correct posix_madvise information, allocating 4 contiguous pages and mapping them together,
    // might be a useful future optimization.

    let mut allow_writable = true;

    let frame = match grant_info.provider {
        Provider::Allocated if access == AccessMode::Write => {
            match faulting_pageinfo_opt {
                Some((_, None)) => unreachable!("allocated page needs frame to be valid"),
                Some((frame, Some(info))) => if info.cow_refcount.load(Ordering::SeqCst) == 1 {
                    frame
                } else {
                    cow(&mut addr_space.table.utable, faulting_page, frame, info, grant_flags)?
                },
                _ => map_zeroed(&mut addr_space.table.utable, faulting_page, grant_flags, true)?,
            }
        }
        Provider::Allocated => {
            match faulting_pageinfo_opt {
                Some((_, None)) => unreachable!("allocated page needs frame to be valid"),
                Some((frame, Some(page_info))) => {
                    allow_writable = page_info.cow_refcount.load(Ordering::SeqCst) == 1;

                    frame
                }

                None => {
                    // TODO: the zeroed page first, readonly?
                    map_zeroed(&mut addr_space.table.utable, faulting_page, grant_flags, false)?
                }
            }
        }
        Provider::PhysBorrowed { base } => {
            base.next_by(pages_from_grant_start)
        }
        Provider::External { address_space: ref foreign_address_space, src_base } => {
            let guard = foreign_address_space.read();
            let src_page = src_base.next_by(pages_from_grant_start);

            if let Some((phys, _)) = guard.table.utable.translate(src_page.start_address()) {
                let src_frame = Frame::containing_address(phys);

                let info = get_page_info(src_frame).expect("all allocated frames need a PageInfo");
                info.add_ref(false);

                src_frame
            } else {
                // TODO: Should this be called?
                map_zeroed(&mut addr_space.table.utable, src_page, grant_flags, access == AccessMode::Write)?
            }
        }
        Provider::Fmap { ref desc } => todo!(),
    };

    if super::context_id().into() == 3 {
        //log::info!("Correcting {:?} => {:?} (base {:?} info {:?})", faulting_page, frame, grant_base, grant_info);
    }
    let new_flags = grant_flags.write(grant_flags.has_write() && allow_writable);
    let Some(flush) = (unsafe { addr_space.table.utable.map_phys(faulting_page.start_address(), frame.start_address(), new_flags) }) else {
        // TODO
        return Err(PfError::Oom);
    };

    flush.flush();

    Ok(())
}

#[cfg(tests)]
mod tests {
    // TODO: Get these tests working
    #[test]
    fn region_collides() {
        assert!(Region::new(0, 2).collides(Region::new(0, 1)));
        assert!(Region::new(0, 2).collides(Region::new(1, 1)));
        assert!(!Region::new(0, 2).collides(Region::new(2, 1)));
        assert!(!Region::new(0, 2).collides(Region::new(3, 1)));
    }
}
